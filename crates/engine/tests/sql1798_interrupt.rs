//! 文の協調的中断（`engine::interrupt`。Issue #1798・SQL-34・WIRE-11 のポインタ）の結合テスト。
//!
//! 「`cap` より少ない一致行の後ろに WHERE の式評価で `22012` を起こす毒行」を置き、最初の
//! `poll` で中断を確定させる。中断の確認点が走査ループの先頭に効いていれば `57014`、毒行まで
//! 走査してしまえば `22012` になる。明示トランザクション内の `Failed` 遷移・autocommit の
//! 副作用ゼロ・スコープ外で挙動が変わらないことも固定する。

use engine::core::EngineCore;
use engine::interrupt::{InterruptReason, InterruptScope};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const CAP: usize = 5;

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(CAP));
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k INTEGER, v INTEGER)",
        "CREATE TABLE u (id BIGINT PRIMARY KEY, k INTEGER, w INTEGER)",
        "CREATE TABLE sink (id BIGINT PRIMARY KEY, k INTEGER, v INTEGER)",
    ] {
        core.execute_sql_in_session(&ctx(), &mut session, ddl)
            .unwrap_or_else(|e| panic!("ddl {ddl}: {e:?}"));
    }
    // 一致行 2 件（cap 未満）の後ろに k = 0 の毒行。
    core.execute_insert_sql(
        &ctx(),
        "INSERT INTO t (id, k, v) VALUES (1, 1, 10), (2, 2, 20), (100, 0, 0)",
    )
    .expect("seed t");
    core.execute_insert_sql(
        &ctx(),
        "INSERT INTO u (id, k, w) VALUES (1, 1, 100), (2, 2, 200)",
    )
    .expect("seed u");
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), sql)
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, sql) {
        Ok(o) => panic!("expected error for {sql}, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

const WHERE_DIV: &str = "WHERE 100 / k > 0";

const QUERIES: &[&str] = &[
    "SELECT * FROM t WHERE 100 / k > 0",
    "SELECT * FROM t WHERE 100 / k > 0 ORDER BY id",
    "SELECT * FROM t WHERE 100 / k > 0 ORDER BY v",
    "SELECT id, ROW_NUMBER() OVER (ORDER BY v) AS rn FROM t WHERE 100 / k > 0",
    "SELECT k, SUM(v) AS s FROM t WHERE 100 / k > 0 GROUP BY k",
    "SELECT SUM(v) AS s FROM t WHERE 100 / k > 0",
];

/// JOIN は入力行の読み込み（relation snapshot）とプローブの両方に確認点がある。毒行ではなく
/// 「中断が確定していれば結果が返らない」ことで固定する。
const JOIN: &str = "SELECT t.id, u.w FROM t JOIN u ON t.k = u.k";

/// スコープ外では毒行まで走査して `22012` になる（挙動が変わらない基準）。
#[test]
fn without_scope_scan_reaches_the_poison_row() {
    let (core, _g) = open("sql1798-baseline");
    for sql in QUERIES {
        assert_eq!(code(&core, sql), "22012", "{sql}");
    }
}

/// スコープ外の JOIN は通常どおり結果を返す。
#[test]
fn join_without_scope_returns_rows() {
    let (core, _g) = open("sql1798-join-base");
    match run(&core, JOIN).expect("join") {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 2),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 最初の `poll` で中断が確定すれば、毒行へ到達せず `57014` になる（部分結果は返らない）。
#[test]
fn interrupt_stops_scan_before_the_poison_row() {
    let (core, _g) = open("sql1798-stop");
    let _scope = InterruptScope::install(Box::new(|| Some(InterruptReason::ClientGone)));
    for sql in QUERIES.iter().copied().chain([JOIN]) {
        assert_eq!(code(&core, sql), "57014", "{sql}");
    }
}

/// ガードを drop すれば元の挙動に戻る（スレッドにスコープが残らない）。
#[test]
fn dropping_the_guard_restores_normal_behavior() {
    let (core, _g) = open("sql1798-drop");
    {
        let _scope = InterruptScope::install(Box::new(|| Some(InterruptReason::ClientGone)));
        assert_eq!(code(&core, "SELECT * FROM t"), "57014");
    }
    match run(&core, "SELECT * FROM t WHERE id < 100").expect("select") {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 2),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 明示トランザクション内の `57014` は `Failed`（`25P02`）にし、`ROLLBACK` で復帰できる。
#[test]
fn canceled_statement_fails_the_explicit_transaction() {
    let (core, _g) = open("sql1798-txn");
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    {
        let _scope = InterruptScope::install(Box::new(|| Some(InterruptReason::ClientGone)));
        let err = core
            .execute_sql_in_txn(&caller, &mut session, &mut txn, "SELECT * FROM t")
            .expect_err("canceled");
        assert_eq!(err.wire_code(), "57014");
    }
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "SELECT * FROM t WHERE id < 100",
        )
        .expect_err("failed transaction");
    assert_eq!(err.wire_code(), "25P02");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    match core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "SELECT * FROM t WHERE id < 100",
        )
        .expect("usable again")
    {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 2),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// autocommit の `INSERT ... SELECT` が中断されても副作用は残らない。
#[test]
fn canceled_autocommit_insert_select_leaves_no_rows() {
    let (core, _g) = open("sql1798-insert-select");
    {
        let _scope = InterruptScope::install(Box::new(|| Some(InterruptReason::ClientGone)));
        let sql = format!("INSERT INTO sink (id, k, v) SELECT id, k, v FROM t {WHERE_DIV}");
        assert_eq!(code(&core, &sql), "57014");
    }
    match run(&core, "SELECT * FROM sink").expect("select sink") {
        SqlOutcome::Query(r) => assert!(r.rows.is_empty(), "no side effect"),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 数千行規模でも stride 経由で中断される（k 回目の `poll` で確定。部分結果は返らない）。
#[test]
fn interrupt_fires_on_a_later_stride() {
    let (core, _g) = open("sql1798-stride");
    for chunk in 0..50 {
        let values: Vec<String> = (0..60)
            .map(|i| {
                let n = 1000 + chunk * 60 + i;
                format!("({n}, {n}, {n})")
            })
            .collect();
        core.execute_insert_sql(
            &ctx(),
            &format!("INSERT INTO sink (id, k, v) VALUES {}", values.join(", ")),
        )
        .expect("seed sink");
    }
    let mut polls = 0u32;
    let _scope = InterruptScope::install(Box::new(move || {
        polls += 1;
        (polls >= 2).then_some(InterruptReason::ClientGone)
    }));
    // cap を超えない形（集約）で 3000 行を走査させる。1024 行ごとの 2 回目の poll で中断する。
    assert_eq!(code(&core, "SELECT SUM(v) AS s FROM sink"), "57014");
}
