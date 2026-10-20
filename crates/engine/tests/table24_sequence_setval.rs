//! `setval` と `lastval`（TABLE-24・SQL-23、Issue #1691）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-24 (1)(2)(4)、
//! `docs/spec/04-behavior/error-format.md` ERR-6。設計記録は
//! `docs/design/sequence-namespace-and-ledger.md` §10。
//!
//! 実 `Storage` ＋ `CpuScalarProvider`、`EngineCore::execute_sql_in_session`／
//! `execute_sql_in_txn` を production 経路として使う。期待値は PostgreSQL 17 の挙動
//! （`do_setval`／`lastval`）に揃える。
//!
//! | 手順 | 期待 |
//! | ---- | ---- |
//! | `CREATE SEQUENCE s; SELECT setval('s', 10)` | 戻り値 10、次の nextval は 11 |
//! | `SELECT setval('s', 10, false)` | 戻り値 10、次の nextval は 10、その次は 11 |
//! | `INCREMENT BY -1` の SEQUENCE へ `setval(-5)` | 次の nextval は -6 |
//! | `MAXVALUE 3` の SEQUENCE へ `setval(3)` | 次の nextval は `2200H`（CYCLE なら 1） |
//! | 範囲外（MIN 未満・MAX 超過・i64 範囲外） | `22003` |
//! | `setval('s', 10)` → `currval('s')` | 10 |
//! | `setval('s', 10, false)` のみ → `currval('s')` | `55000` |
//! | `setval` のみ → `lastval()` | `55000` |
//! | `nextval('a')` → `nextval('b')` → `lastval()` | b の値 |
//! | `nextval('s')` → `setval('s', 50)` → `lastval()` | 50 |
//! | nextval → DROP SEQUENCE → `lastval()` | `55000`（同名で作り直しても `55000`） |
//! | nextval → RENAME → `lastval()` | 値を返す（内部 ID で照合） |

use engine::catalog::ColumnType;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn open_core(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
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

fn ok(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) -> SqlOutcome {
    run(core, tenant, session, sql).unwrap_or_else(|e| panic!("{sql} must succeed: {e}"))
}

fn code(core: &EngineCore, tenant: &str, session: &mut SessionState, sql: &str) -> &'static str {
    match run(core, tenant, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

/// 1 行 1 列の bigint 結果から値を取り出す。
fn scalar(out: SqlOutcome) -> i64 {
    match out {
        SqlOutcome::Query(q) => {
            assert_eq!(q.rows.len(), 1);
            match q.rows[0].cells.as_slice() {
                [Cell::SignedInteger(v)] => *v,
                other => panic!("unexpected cells {other:?}"),
            }
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

fn sv(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> i64 {
    scalar(ok(core, tenant, s, sql))
}

/// `nextval(seq)` を `INSERT ... RETURNING` で 1 つ払い出して観測する（autocommit）。
fn nv(core: &EngineCore, tenant: &str, s: &mut SessionState, seq: &str) -> i64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let sql = format!(
        "INSERT INTO t (id, n) VALUES ({id}, nextval('{seq}')) RETURNING n \
         USING OPERATION_ID 'op-{id}'"
    );
    match ok(core, tenant, s, &sql) {
        SqlOutcome::Returning(o) => match o.result.rows[0].cells[0] {
            Cell::SignedInteger(v) => v,
            ref c => panic!("cell {c:?}"),
        },
        other => panic!("expected Returning, got {other:?}"),
    }
}

fn nv_err(core: &EngineCore, tenant: &str, s: &mut SessionState, seq: &str) -> &'static str {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let sql = format!(
        "INSERT INTO t (id, n) VALUES ({id}, nextval('{seq}')) USING OPERATION_ID 'op-{id}'"
    );
    code(core, tenant, s, &sql)
}

fn insert_sql(seq: &str) -> String {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("INSERT INTO t (id, n) VALUES ({id}, nextval('{seq}')) USING OPERATION_ID 'op-{id}'")
}

fn fixture(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore, SessionState) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open_core(&path);
    let mut s = ddl_session();
    ok(&core, "sys", &mut s, "CREATE SEQUENCE s");
    ok(
        &core,
        "sys",
        &mut s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT)",
    );
    (path, guard, core, s)
}

#[test]
fn setval_then_nextval_continues_from_the_value() {
    let (_p, _g, core, mut s) = fixture("setval-basic");
    assert_eq!(sv(&core, "alice", &mut s, "SELECT setval('s', 10)"), 10);
    assert_eq!(nv(&core, "alice", &mut s, "s"), 11);
    assert_eq!(nv(&core, "alice", &mut s, "s"), 12);
}

#[test]
fn setval_is_called_false_makes_the_value_the_next_one() {
    let (_p, _g, core, mut s) = fixture("setval-false");
    assert_eq!(
        sv(&core, "alice", &mut s, "SELECT setval('s', 10, false)"),
        10
    );
    assert_eq!(nv(&core, "alice", &mut s, "s"), 10);
    assert_eq!(nv(&core, "alice", &mut s, "s"), 11);
    assert_eq!(
        sv(&core, "alice", &mut s, "SELECT setval('s', 20, TRUE)"),
        20
    );
    assert_eq!(nv(&core, "alice", &mut s, "s"), 21);
}

#[test]
fn setval_on_descending_and_bounded_sequences() {
    let (_p, _g, core, mut s) = fixture("setval-bounds");
    ok(&core, "sys", &mut s, "CREATE SEQUENCE d INCREMENT BY -1");
    assert_eq!(sv(&core, "alice", &mut s, "SELECT setval('d', -5)"), -5);
    assert_eq!(nv(&core, "alice", &mut s, "d"), -6);

    ok(&core, "sys", &mut s, "CREATE SEQUENCE m MAXVALUE 3");
    assert_eq!(sv(&core, "alice", &mut s, "SELECT setval('m', 3)"), 3);
    assert_eq!(nv_err(&core, "alice", &mut s, "m"), "2200H");
    // is_called = false なら最大値そのものが次に出る。
    assert_eq!(
        sv(&core, "alice", &mut s, "SELECT setval('m', 3, false)"),
        3
    );
    assert_eq!(nv(&core, "alice", &mut s, "m"), 3);

    ok(&core, "sys", &mut s, "CREATE SEQUENCE c MAXVALUE 3 CYCLE");
    assert_eq!(sv(&core, "alice", &mut s, "SELECT setval('c', 3)"), 3);
    assert_eq!(nv(&core, "alice", &mut s, "c"), 1);
}

#[test]
fn setval_out_of_range_is_22003_and_changes_nothing() {
    let (_p, _g, core, mut s) = fixture("setval-range");
    ok(&core, "sys", &mut s, "CREATE SEQUENCE m MAXVALUE 3");
    for sql in [
        "SELECT setval('s', 0)",
        "SELECT setval('m', 4)",
        "SELECT setval('s', -1)",
        "SELECT setval('s', 9223372036854775808)",
        "SELECT setval('s', -9223372036854775809)",
        "SELECT setval('s', 99999999999999999999999999)",
    ] {
        assert_eq!(code(&core, "alice", &mut s, sql), "22003", "{sql}");
    }
    assert_eq!(nv(&core, "alice", &mut s, "s"), 1);
    assert_eq!(
        sv(
            &core,
            "alice",
            &mut s,
            "SELECT setval('s', 9223372036854775807)"
        ),
        i64::MAX
    );
    assert_eq!(nv_err(&core, "alice", &mut s, "s"), "2200H");
}

#[test]
fn currval_follows_setval_only_when_is_called() {
    let (_p, _g, core, mut s) = fixture("setval-currval");
    sv(&core, "alice", &mut s, "SELECT setval('s', 10, false)");
    assert_eq!(
        code(
            &core,
            "alice",
            &mut s,
            "INSERT INTO t (id, n) VALUES (900, currval('s')) USING OPERATION_ID 'cv0'"
        ),
        "55000"
    );
    sv(&core, "alice", &mut s, "SELECT setval('s', 10)");
    ok(
        &core,
        "alice",
        &mut s,
        "INSERT INTO t (id, n) VALUES (901, currval('s')) USING OPERATION_ID 'cv1'",
    );
    let r = core
        .execute_sql(&ctx("alice"), "SELECT id, n FROM t LIMIT 10")
        .expect("select");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].cells[1], Cell::SignedInteger(10));
}

#[test]
fn lastval_rules() {
    let (_p, _g, core, mut s) = fixture("lastval-rules");
    ok(&core, "sys", &mut s, "CREATE SEQUENCE a");
    ok(&core, "sys", &mut s, "CREATE SEQUENCE b START 100");

    // nextval 未呼び出し（setval のみ）は 55000。
    assert_eq!(code(&core, "alice", &mut s, "SELECT lastval()"), "55000");
    sv(&core, "alice", &mut s, "SELECT setval('a', 7)");
    assert_eq!(code(&core, "alice", &mut s, "SELECT lastval()"), "55000");

    // 直近に nextval した SEQUENCE の値。
    assert_eq!(nv(&core, "alice", &mut s, "a"), 8);
    assert_eq!(nv(&core, "alice", &mut s, "b"), 100);
    assert_eq!(sv(&core, "alice", &mut s, "SELECT lastval()"), 100);
    assert_eq!(sv(&core, "alice", &mut s, "SELECT LASTVAL() AS v;"), 100);

    // setval(is_called) は currval を上書きするが lastval の対象は変えない。
    sv(&core, "alice", &mut s, "SELECT setval('a', 50)");
    assert_eq!(sv(&core, "alice", &mut s, "SELECT lastval()"), 100);
    // 対象の SEQUENCE への setval は lastval に反映される。
    sv(&core, "alice", &mut s, "SELECT setval('b', 500)");
    assert_eq!(sv(&core, "alice", &mut s, "SELECT lastval()"), 500);

    // 別セッションは未定義。別テナントの nextval は見えない。
    let mut other = ddl_session();
    assert_eq!(
        code(&core, "alice", &mut other, "SELECT lastval()"),
        "55000"
    );
    assert_eq!(nv(&core, "bob", &mut other, "a"), 1);
    assert_eq!(sv(&core, "bob", &mut other, "SELECT lastval()"), 1);
    // 同じセッションを別テナントの文脈で使っても他テナントの値は見えない。
    assert_eq!(
        code(&core, "carol", &mut other, "SELECT lastval()"),
        "55000"
    );
}

#[test]
fn nextval_then_setval_then_lastval_returns_the_set_value() {
    let (_p, _g, core, mut s) = fixture("lastval-after-setval");
    assert_eq!(nv(&core, "alice", &mut s, "s"), 1);
    sv(&core, "alice", &mut s, "SELECT setval('s', 50)");
    assert_eq!(sv(&core, "alice", &mut s, "SELECT lastval()"), 50);
}

#[test]
fn lastval_after_drop_is_55000_even_if_recreated() {
    let (_p, _g, core, mut s) = fixture("lastval-drop");
    assert_eq!(nv(&core, "alice", &mut s, "s"), 1);
    ok(&core, "sys", &mut s, "DROP SEQUENCE s");
    assert_eq!(code(&core, "alice", &mut s, "SELECT lastval()"), "55000");
    ok(&core, "sys", &mut s, "CREATE SEQUENCE s");
    assert_eq!(code(&core, "alice", &mut s, "SELECT lastval()"), "55000");

    // 作り直した後に nextval すれば有効になる。
    assert_eq!(nv(&core, "alice", &mut s, "s"), 1);
    ok(&core, "sys", &mut s, "DROP SEQUENCE s");
    ok(&core, "sys", &mut s, "CREATE SEQUENCE s");
    assert_eq!(code(&core, "alice", &mut s, "SELECT lastval()"), "55000");
}

#[test]
fn lastval_survives_rename() {
    let (_p, _g, core, mut s) = fixture("lastval-rename");
    assert_eq!(nv(&core, "alice", &mut s, "s"), 1);
    ok(&core, "sys", &mut s, "ALTER SEQUENCE s RENAME TO s2");
    assert_eq!(sv(&core, "alice", &mut s, "SELECT lastval()"), 1);
    assert_eq!(sv(&core, "alice", &mut s, "SELECT setval('s2', 9)"), 9);
    assert_eq!(nv(&core, "alice", &mut s, "s2"), 10);
}

#[test]
fn unknown_and_wrong_kind_names_are_uniform_across_tenants() {
    let (_p, _g, core, mut s) = fixture("setval-names");
    for tenant in ["alice", "bob", "carol"] {
        assert_eq!(
            code(&core, tenant, &mut s, "SELECT setval('nothere', 1)"),
            "42P01"
        );
        assert_eq!(
            code(&core, tenant, &mut s, "SELECT setval('t', 1)"),
            "42809"
        );
    }
}

#[test]
fn tenants_are_independent() {
    let (_p, _g, core, mut s) = fixture("setval-tenants");
    sv(&core, "alice", &mut s, "SELECT setval('s', 100)");
    assert_eq!(nv(&core, "bob", &mut s, "s"), 1);
    assert_eq!(nv(&core, "alice", &mut s, "s"), 101);
    sv(&core, "bob", &mut s, "SELECT setval('s', 5, false)");
    assert_eq!(nv(&core, "alice", &mut s, "s"), 102);
    assert_eq!(nv(&core, "bob", &mut s, "s"), 5);
}

#[test]
fn setval_does_not_need_ddl_permission() {
    let (_p, _g, core, _s) = fixture("setval-no-ddl");
    let mut plain = SessionState::default();
    assert_eq!(sv(&core, "alice", &mut plain, "SELECT setval('s', 3)"), 3);
    assert_eq!(nv(&core, "alice", &mut plain, "s"), 4);
    assert_eq!(sv(&core, "alice", &mut plain, "SELECT lastval()"), 4);
}

#[test]
fn autocommit_setval_is_durable_across_reopen() {
    let (p, _g, core, mut s) = fixture("setval-reopen");
    sv(&core, "alice", &mut s, "SELECT setval('s', 100)");
    drop(core);
    let core = open_core(&p);
    let mut s2 = ddl_session();
    assert_eq!(nv(&core, "alice", &mut s2, "s"), 101);
}

#[test]
fn alias_and_column_metadata() {
    let (_p, _g, core, mut s) = fixture("setval-columns");
    for (sql, name) in [
        ("SELECT setval('s', 1)", "setval"),
        ("SELECT setval('s', 1) AS x", "x"),
        ("SELECT lastval() AS y", "y"),
    ] {
        let out = run(&core, "alice", &mut s, sql);
        if sql.contains("lastval") {
            assert_eq!(out.unwrap_err().wire_code(), "55000");
            continue;
        }
        match out.expect("ok") {
            SqlOutcome::Query(q) => match &q.columns[0] {
                ColumnMeta::Computed { name: n, ty } => {
                    assert_eq!(n, name);
                    assert_eq!(*ty, Some(ColumnType::BigInt));
                }
                other => panic!("column {other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }
}

fn txn_run<'a>(
    core: &'a EngineCore,
    tenant: &str,
    session: &mut SessionState,
    txn: &mut engine::sql::transaction::SessionTransaction<'a>,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_txn(&ctx(tenant), session, txn, sql)
}

#[test]
fn setval_only_transaction_commit_is_durable() {
    let (p, _g, core, _s) = fixture("setval-txn-only");
    {
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        for sql in ["BEGIN", "SELECT setval('s', 100)", "COMMIT"] {
            txn_run(&core, "alice", &mut session, &mut txn, sql)
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
    }
    drop(core);
    let core = open_core(&p);
    let mut s2 = ddl_session();
    assert_eq!(nv(&core, "alice", &mut s2, "s"), 101);
}

#[test]
fn setval_in_transaction_is_seen_by_following_nextval_and_persists() {
    let (p, _g, core, _s) = fixture("setval-txn-next");
    {
        let mut session = SessionState::default();
        let mut txn = core.new_session_transaction();
        txn_run(&core, "alice", &mut session, &mut txn, "BEGIN").expect("begin");
        let out = txn_run(
            &core,
            "alice",
            &mut session,
            &mut txn,
            "SELECT setval('s', 100)",
        )
        .expect("setval");
        assert_eq!(scalar(out), 100);
        txn_run(&core, "alice", &mut session, &mut txn, &insert_sql("s")).expect("insert");
        let last =
            txn_run(&core, "alice", &mut session, &mut txn, "SELECT lastval()").expect("lastval");
        assert_eq!(scalar(last), 101);
        txn_run(&core, "alice", &mut session, &mut txn, "COMMIT").expect("commit");
    }
    drop(core);
    let core = open_core(&p);
    let mut s2 = ddl_session();
    assert_eq!(nv(&core, "alice", &mut s2, "s"), 102);
}

#[test]
fn rolled_back_setval_keeps_the_in_process_value() {
    let (_p, _g, core, _s) = fixture("setval-rollback");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in ["BEGIN", "SELECT setval('s', 100)", "ROLLBACK"] {
        txn_run(&core, "alice", &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // PostgreSQL の setval は非トランザクション的で、同じプロセスでは ROLLBACK 後も続きの値が出る。
    let mut s2 = ddl_session();
    assert_eq!(nv(&core, "alice", &mut s2, "s"), 101);
}

#[test]
fn failed_setval_in_transaction_fails_the_transaction_and_changes_nothing() {
    let (_p, _g, core, _s) = fixture("setval-txn-fail");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    txn_run(&core, "alice", &mut session, &mut txn, "BEGIN").expect("begin");
    let err = txn_run(
        &core,
        "alice",
        &mut session,
        &mut txn,
        "SELECT setval('s', 0)",
    )
    .expect_err("out of range");
    assert_eq!(err.wire_code(), "22003");
    let err = txn_run(
        &core,
        "alice",
        &mut session,
        &mut txn,
        "SELECT setval('s', 5)",
    )
    .expect_err("txn is failed");
    assert_eq!(err.wire_code(), "25P02");
    txn_run(&core, "alice", &mut session, &mut txn, "ROLLBACK").expect("rollback");
    let mut s2 = ddl_session();
    assert_eq!(nv(&core, "alice", &mut s2, "s"), 1);
}

/// 単独形以外は実行されず、`0A000`（単独形の近傍）／`42601`（式位置など許可リスト外の関数呼び出し）／
/// `42723`（UDF 名の予約）で閉じる。副作用（次の nextval の値）が変わらない。
#[test]
fn unsupported_positions_are_rejected_without_side_effects() {
    let (_p, _g, core, mut s) = fixture("setval-reject");
    let mut mismatches: Vec<String> = Vec::new();
    for (sql, expected) in [
        ("SELECT 1, setval('s', 50)", "0A000"),
        ("SELECT setval('s', 50), 1", "0A000"),
        ("SELECT setval('s', 50) FROM t", "0A000"),
        ("SELECT setval('s', 50) WHERE 1 = 1", "0A000"),
        ("SELECT setval('s', 50) LIMIT 1", "0A000"),
        ("SELECT setval('s', 50) + 1", "0A000"),
        ("SELECT setval('s', 50) AS a, 2", "0A000"),
        ("SELECT setval('s', 50)::bigint", "0A000"),
        ("SELECT setval('s', 50); SELECT 1", "0A000"),
        ("SELECT lastval() FROM t", "0A000"),
        ("SELECT lastval() + 1", "0A000"),
        ("SELECT setval('s', NULL)", "0A000"),
        ("SELECT setval('s', 1 + 1)", "0A000"),
        ("SELECT setval('s', 50, NULL)", "0A000"),
        ("SELECT setval('s', 50, 1)", "0A000"),
        ("SELECT setval('public.s', 50)", "0A000"),
        ("SELECT setval('\"s\"', 50)", "0A000"),
        ("SELECT setval(NULL, 50)", "42601"),
        ("SELECT setval('s', 1.5)", "42601"),
        ("SELECT setval('s', '5')", "42601"),
        ("SELECT setval('s')", "42601"),
        ("SELECT setval()", "42601"),
        ("SELECT setval('s', 5, true, 1)", "0A000"),
        ("SELECT lastval(1)", "42601"),
        ("SELECT setval('bad name', 50)", "42601"),
        ("SELECT n, setval('s', 50) FROM t", "42601"),
        ("SELECT * FROM t WHERE n = setval('s', 50)", "42601"),
        ("UPDATE t SET n = setval('s', 50) WHERE id = 1", "42601"),
        (
            "INSERT INTO t (id, n) VALUES (1, setval('s', 50)) USING OPERATION_ID 'rj1'",
            "42601",
        ),
        (
            "INSERT INTO t (id, n) VALUES (2, lastval()) USING OPERATION_ID 'rj2'",
            "42601",
        ),
        ("CREATE FUNCTION setval(x) AS x + 1", "42723"),
        ("CREATE VIEW v AS SELECT setval('s', 50)", "42601"),
    ] {
        let got = code(&core, "alice", &mut s, sql);
        if got != expected {
            mismatches.push(format!("{sql}: got {got}, want {expected}"));
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
    // どれも副作用を起こしていない。
    assert_eq!(nv(&core, "alice", &mut s, "s"), 1);
}

#[test]
fn explain_and_cursor_forms_do_not_execute_setval() {
    let (_p, _g, core, mut s) = fixture("setval-explain");
    for sql in [
        "EXPLAIN SELECT setval('s', 50)",
        "DECLARE c CURSOR FOR SELECT setval('s', 50)",
        "WITH x AS (SELECT setval('s', 50)) SELECT * FROM x",
    ] {
        let got = code(&core, "alice", &mut s, sql);
        assert!(
            ["0A000", "42601", "42883"].contains(&got),
            "{sql}: unexpected {got}"
        );
    }
    assert_eq!(nv(&core, "alice", &mut s, "s"), 1);
}

#[test]
fn works_in_a_session_that_has_temp_tables() {
    let (_p, _g, core, mut s) = fixture("setval-temp");
    ok(
        &core,
        "alice",
        &mut s,
        "CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY, a BIGINT)",
    );
    assert_eq!(sv(&core, "alice", &mut s, "SELECT setval('s', 10)"), 10);
    assert_eq!(nv(&core, "alice", &mut s, "s"), 11);
    assert_eq!(sv(&core, "alice", &mut s, "SELECT lastval()"), 11);
}
