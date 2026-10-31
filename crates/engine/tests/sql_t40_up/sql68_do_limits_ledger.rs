//! `DO` ブロック（SQL-68）の資源上限超過の fail-closed と、`operation_id` 台帳との整合の結合テスト
//! （Issue #2229）。ポインタ: `docs/spec/04-behavior/sql.md` SQL-68、RECOVER-11・RECOVER-12。
//! 設計記録は `docs/design/plpgsql-subset.md`「Issue #2229 追記」。
//!
//! 実 `Storage` ＋ `CpuScalarProvider` の production 経路（`execute_sql_in_session`／
//! `execute_sql_in_txn`）で次を固定する。
//! - 文数・ループ回数・入れ子の深さ・本体長・結果行数の上限超過は、文全体を拒否し部分永続を残さない
//!   （autocommit・明示 Tx の両方。上限ちょうどは成功し +1 で失敗する）。エラー文言は他テナントの
//!   データ有無で変わらない。
//! - `DO` は `USING OPERATION_ID` を受理しない（`0A000`）。受理して記録しないまま二重実行する
//!   fail-open にならない。`Ledgered` 構成の本体の書き込みは `23502` で副作用なし。
//! - 明示 Tx の証人 INSERT（`USING OPERATION_ID`）と同じコミットに載せた `DO` は、再送されても
//!   副作用が重複しない（証人が先でも `DO` が先でも。再オープン後も同じ）。
//!
//! 呼び出し深さ（`54001`）は本番 SQL から PL/pgSQL 関数を呼ぶ配線が無い（#2217・#2218 待ち）ため、
//! 本ファイルの対象外（`plpgsql/interp_tests.rs` の ScriptedHost で固定済み）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::plpgsql_limits::PlpgsqlLimits;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::result_limits::ResultLimits;
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

/// 上限・台帳モードを指定してエンジンを作り、表 `t (n INTEGER)`・`w (k BIGINT)` を用意する。
fn build(
    path: &std::path::Path,
    limits: Option<PlpgsqlLimits>,
    results: Option<ResultLimits>,
    mode: LedgerMode,
) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    let mut core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_ledger_mode(mode);
    if let Some(l) = limits {
        core = core.with_plpgsql_limits(l);
    }
    if let Some(r) = results {
        core = core.with_result_limits(r);
    }
    core
}

fn create_tables(core: &EngineCore) {
    let mut s = SessionState::default();
    s.allow_ddl();
    for sql in ["CREATE TABLE t (n INTEGER)", "CREATE TABLE w (k BIGINT)"] {
        core.execute_sql_in_session(&ctx("alice"), &mut s, sql)
            .expect("create table");
    }
}

fn setup(
    label: &str,
    limits: Option<PlpgsqlLimits>,
    results: Option<ResultLimits>,
    mode: LedgerMode,
) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let core = build(&path, limits, results, mode);
    create_tables(&core);
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

/// 失敗する文の `(wire_code, 文言)`。
fn fail(core: &EngineCore, tenant: &str, sql: &str) -> (String, String) {
    match run(core, tenant, sql) {
        Ok(o) => panic!("must fail: {sql} -> {o:?}"),
        Err(e) => (e.wire_code().to_string(), e.to_string()),
    }
}

fn count(core: &EngineCore, tenant: &str, table: &str) -> u64 {
    let r = core
        .execute_sql(&ctx(tenant), &format!("SELECT COUNT(*) FROM {table}"))
        .expect("count");
    match r.rows[0].cells[0] {
        Cell::Integer(v) => v,
        ref o => panic!("unexpected {o:?}"),
    }
}

/// 本体文字列を `DO '<本体>'` に包む（本体内の `'` は二重化する）。
fn do_lit(body: &str) -> String {
    format!("DO '{}'", body.replace('\'', "''"))
}

fn inserts(n: usize) -> String {
    "INSERT INTO t (n) VALUES (1); ".repeat(n)
}

fn small_limits() -> PlpgsqlLimits {
    // 文数 5・ループ 4・入れ子 3・本体 300 バイト・呼び出し深さ 2。
    PlpgsqlLimits::new(5, 4, 3, 300, 2)
}

fn recorded(core: &EngineCore, tenant: &str, op: &str) -> bool {
    let op = OperationId::parse(op).expect("op id");
    matches!(
        core.operation_recorded(&ctx(tenant), "w", &op)
            .expect("lookup"),
        LedgerLookup::Recorded
    )
}

// ---- 資源上限（fail-closed・部分永続なし） ----

#[test]
fn statement_limit_is_exact_and_fails_closed() {
    let (core, _g) = setup(
        "sql68-lim-stmt",
        Some(small_limits()),
        None,
        LedgerMode::Optional,
    );
    // 他テナントのデータの有無でエラー文言が変わらない（存在情報を漏らさない）。
    let before = fail(
        &core,
        "alice",
        &do_lit(&format!("BEGIN {} END", inserts(6))),
    );
    run(&core, "bob", "INSERT INTO t (n) VALUES (9)").expect("bob seed");
    let after = fail(
        &core,
        "alice",
        &do_lit(&format!("BEGIN {} END", inserts(6))),
    );
    assert_eq!(before.0, "54000");
    assert_eq!(before, after);
    assert_eq!(count(&core, "alice", "t"), 0, "no partial persistence");
    // 上限ちょうどは成功する。
    run(
        &core,
        "alice",
        &do_lit(&format!("BEGIN {} END", inserts(5))),
    )
    .expect("exactly the limit");
    assert_eq!(count(&core, "alice", "t"), 5);
    // +1 で失敗し、行数は成功分のまま。他テナントの行も不変。
    assert_eq!(
        fail(
            &core,
            "alice",
            &do_lit(&format!("BEGIN {} END", inserts(6)))
        )
        .0,
        "54000"
    );
    assert_eq!(count(&core, "alice", "t"), 5);
    assert_eq!(count(&core, "bob", "t"), 1);
}

#[test]
fn loop_iteration_limit_applies_to_every_loop_form() {
    let (core, _g) = setup(
        "sql68-lim-loop",
        Some(small_limits()),
        None,
        LedgerMode::Optional,
    );
    let ins = "INSERT INTO t (n) VALUES (1);";
    let bodies = [
        format!("BEGIN LOOP {ins} END LOOP; END"),
        format!("DECLARE s int := 0; BEGIN WHILE s < 100 LOOP {ins} s := s + 1; END LOOP; END"),
        format!("BEGIN FOR i IN 1..100 LOOP {ins} END LOOP; END"),
    ];
    for b in &bodies {
        assert_eq!(fail(&core, "alice", &do_lit(b)).0, "54000", "{b}");
        assert_eq!(count(&core, "alice", "t"), 0, "no partial rows: {b}");
    }
    // 上限ちょうど（4 回）は成功し、5 回は失敗する。
    run(
        &core,
        "alice",
        &do_lit(&format!("BEGIN FOR i IN 1..4 LOOP {ins} END LOOP; END")),
    )
    .expect("exactly the loop limit");
    assert_eq!(count(&core, "alice", "t"), 4);
    assert_eq!(
        fail(
            &core,
            "alice",
            &do_lit(&format!("BEGIN FOR i IN 1..5 LOOP {ins} END LOOP; END"))
        )
        .0,
        "54000"
    );
    assert_eq!(count(&core, "alice", "t"), 4);
}

#[test]
fn nesting_depth_limit_is_54001_without_side_effects() {
    let (core, _g) = setup(
        "sql68-lim-nest",
        Some(small_limits()),
        None,
        LedgerMode::Optional,
    );
    let nest = |n: usize| {
        format!(
            "{}INSERT INTO t (n) VALUES (1); {}",
            "BEGIN ".repeat(n),
            "END; ".repeat(n)
        )
    };
    run(&core, "alice", &do_lit(&nest(3))).expect("exactly the nesting limit");
    assert_eq!(count(&core, "alice", "t"), 1);
    assert_eq!(fail(&core, "alice", &do_lit(&nest(4))).0, "54001");
    let nested_if = "BEGIN IF 1 = 1 THEN IF 1 = 1 THEN IF 1 = 1 THEN \
                     INSERT INTO t (n) VALUES (1); END IF; END IF; END IF; END";
    assert_eq!(fail(&core, "alice", &do_lit(nested_if)).0, "54001");
    assert_eq!(count(&core, "alice", "t"), 1, "rejected before any write");
}

#[test]
fn body_size_limit_rejects_before_any_write() {
    let (core, _g) = setup(
        "sql68-lim-body",
        Some(small_limits()),
        None,
        LedgerMode::Optional,
    );
    let big = format!("BEGIN {} END", inserts(20));
    assert_eq!(fail(&core, "alice", &do_lit(&big)).0, "54000");
    assert_eq!(count(&core, "alice", "t"), 0);
    // 陽性対照: 同じ本体は既定の上限のエンジンでは成功する（上限が原因であることの確認）。
    let (plain, _g2) = setup("sql68-lim-body-ok", None, None, LedgerMode::Optional);
    run(&plain, "alice", &do_lit(&big)).expect("default limits accept the same body");
    assert_eq!(count(&plain, "alice", "t"), 20);
}

#[test]
fn positive_control_same_bodies_succeed_with_default_limits() {
    let (core, _g) = setup("sql68-lim-ctrl", None, None, LedgerMode::Optional);
    run(
        &core,
        "alice",
        &do_lit(&format!("BEGIN {} END", inserts(6))),
    )
    .expect("6 statements");
    run(
        &core,
        "alice",
        &do_lit("BEGIN FOR i IN 1..5 LOOP INSERT INTO t (n) VALUES (1); END LOOP; END"),
    )
    .expect("5 iterations");
    assert_eq!(count(&core, "alice", "t"), 11);
}

#[test]
fn limit_violation_in_explicit_transaction_leaves_nothing_after_rollback() {
    let (core, _g) = setup(
        "sql68-lim-txn",
        Some(small_limits()),
        None,
        LedgerMode::Optional,
    );
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    let c = ctx("alice");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "INSERT INTO t (n) VALUES (7)")
        .expect("outer insert");
    let e = core
        .execute_sql_in_txn(
            &c,
            &mut s,
            &mut txn,
            &do_lit(&format!("BEGIN {} END", inserts(6))),
        )
        .expect_err("statement limit");
    assert_eq!(e.wire_code(), "54000");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT COUNT(*) FROM t")
        .expect_err("transaction is failed");
    assert_eq!(e.wire_code(), "25P02");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, "alice", "t"), 0, "outer row is gone too");
}

#[test]
fn result_row_limit_inside_the_body_matches_plain_sql() {
    let (core, _g) = setup(
        "sql68-lim-rows",
        None,
        Some(ResultLimits::new(2)),
        LedgerMode::Optional,
    );
    for _ in 0..3 {
        run(&core, "alice", "INSERT INTO t (n) VALUES (1)").expect("seed");
    }
    let plain = fail(&core, "alice", "SELECT n FROM t");
    let body = "BEGIN FOR r IN SELECT n FROM t LOOP INSERT INTO w (k) VALUES (1); END LOOP; END";
    let via_do = fail(&core, "alice", &do_lit(body));
    assert_eq!(
        plain.0, via_do.0,
        "same SQLSTATE as the equivalent plain SQL"
    );
    assert_eq!(count(&core, "alice", "w"), 0, "no partial results");
}

// ---- operation_id 台帳 ----

#[test]
fn do_rejects_operation_id_without_recording_or_executing() {
    let (core, _g) = setup("sql68-led-reject", None, None, LedgerMode::Optional);
    let body = do_lit("BEGIN INSERT INTO t (n) VALUES (1); END");
    let sql = format!("{body} USING OPERATION_ID 'x'");
    assert_eq!(fail(&core, "alice", &sql).0, "0A000");
    assert_eq!(count(&core, "alice", "t"), 0, "not executed");
    assert!(!recorded(&core, "alice", "x"), "not recorded either");
    // 本体内の EXECUTE ... USING OPERATION_ID も同様に拒否され、副作用はない。
    let nested = do_lit(
        "BEGIN INSERT INTO t (n) VALUES (1); \
         EXECUTE 'INSERT INTO w (k) VALUES (1) USING OPERATION_ID ''y'''; END",
    );
    assert_eq!(fail(&core, "alice", &nested).0, "0A000");
    assert_eq!(count(&core, "alice", "t"), 0);
    assert_eq!(count(&core, "alice", "w"), 0);
    assert!(!recorded(&core, "alice", "y"));
}

#[test]
fn ledgered_mode_allows_reads_and_rejects_writes_without_side_effects() {
    let (core, _g) = setup("sql68-led-ledgered", None, None, LedgerMode::Ledgered);
    run(
        &core,
        "alice",
        &do_lit("DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM t; END"),
    )
    .expect("read-only DO");
    // 読み取りを数文行った後の書き込みでも、途中の状態を残さない。
    let body = "DECLARE c bigint; BEGIN SELECT count(*) INTO c FROM t; \
                SELECT count(*) INTO c FROM w; INSERT INTO t (n) VALUES (1); END";
    assert_eq!(fail(&core, "alice", &do_lit(body)).0, "23502");
    assert_eq!(count(&core, "alice", "t"), 0);
}

/// 明示 Tx で文列を送る（`BEGIN` ... 最後の文）。各文の結果の SQLSTATE（成功は `ok`）を返す。
fn txn_script(core: &EngineCore, tenant: &str, stmts: &[String]) -> Vec<String> {
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    let c = ctx(tenant);
    stmts
        .iter()
        .map(
            |st| match core.execute_sql_in_txn(&c, &mut s, &mut txn, st) {
                Ok(_) => "ok".to_string(),
                Err(e) => e.wire_code().to_string(),
            },
        )
        .collect()
}

fn witness(k: u32, op: &str) -> String {
    format!("INSERT INTO w (k) VALUES ({k}) USING OPERATION_ID '{op}'")
}

const DO_WRITE: &str = "BEGIN INSERT INTO t (n) VALUES (1); INSERT INTO t (n) VALUES (2); END";

#[test]
fn witness_before_do_commits_once_and_resend_is_rejected() {
    let (core, _g) = setup("sql68-led-order-a", None, None, LedgerMode::Optional);
    let script = |op: &str| {
        vec![
            "BEGIN".to_string(),
            witness(1, op),
            do_lit(DO_WRITE),
            "COMMIT".to_string(),
        ]
    };
    assert_eq!(txn_script(&core, "alice", &script("k1")), ["ok"; 4]);
    assert!(recorded(&core, "alice", "k1"));
    assert_eq!(count(&core, "alice", "t"), 2, "exactly one application");
    assert_eq!(count(&core, "alice", "w"), 1);
    // 再送: 台帳由来の 23505 で Tx が失敗し、続く DO は 25P02。ROLLBACK 後も副作用は 1 回分。
    let mut resend = script("k1");
    resend.pop();
    resend.push("ROLLBACK".to_string());
    assert_eq!(
        txn_script(&core, "alice", &resend),
        ["ok", "23505", "25P02", "ok"]
    );
    assert_eq!(count(&core, "alice", "t"), 2);
    assert_eq!(count(&core, "alice", "w"), 1);
}

#[test]
fn do_before_witness_is_discarded_as_a_whole_on_resend() {
    let (core, _g) = setup("sql68-led-order-b", None, None, LedgerMode::Optional);
    let first = vec![
        "BEGIN".to_string(),
        do_lit(DO_WRITE),
        witness(1, "k2"),
        "COMMIT".to_string(),
    ];
    assert_eq!(txn_script(&core, "alice", &first), ["ok"; 4]);
    assert_eq!(count(&core, "alice", "t"), 2);
    // 再送: DO は走るが、続く証人 INSERT が 23505 になり、Tx 全体（DO の書き込みを含む）が破棄される。
    let resend = vec![
        "BEGIN".to_string(),
        do_lit(DO_WRITE),
        witness(1, "k2"),
        "ROLLBACK".to_string(),
    ];
    assert_eq!(
        txn_script(&core, "alice", &resend),
        ["ok", "ok", "23505", "ok"]
    );
    assert_eq!(count(&core, "alice", "t"), 2, "the resent DO left no rows");
    assert_eq!(count(&core, "alice", "w"), 1);
}

#[test]
fn failed_do_leaves_no_ledger_entry_and_the_retry_applies_once() {
    let (core, _g) = setup("sql68-led-retry", None, None, LedgerMode::Optional);
    let failing = vec![
        "BEGIN".to_string(),
        witness(1, "k3"),
        do_lit("BEGIN INSERT INTO t (n) VALUES (1); RAISE EXCEPTION ''x''; END"),
        "ROLLBACK".to_string(),
    ];
    assert_eq!(
        txn_script(&core, "alice", &failing),
        ["ok", "ok", "0A000", "ok"]
    );
    assert!(!recorded(&core, "alice", "k3"), "rolled back with the Tx");
    assert_eq!(count(&core, "alice", "t"), 0);
    assert_eq!(count(&core, "alice", "w"), 0);
    let retry = vec![
        "BEGIN".to_string(),
        witness(1, "k3"),
        do_lit(DO_WRITE),
        "COMMIT".to_string(),
    ];
    assert_eq!(txn_script(&core, "alice", &retry), ["ok"; 4]);
    assert!(recorded(&core, "alice", "k3"));
    assert_eq!(count(&core, "alice", "t"), 2, "exactly one application");
}

#[test]
fn resend_after_reopen_is_still_rejected_and_the_ledger_is_per_tenant() {
    let path = unique_db_path("sql68-led-reopen");
    let _g = CleanupGuard(path.clone());
    {
        let core = build(&path, None, None, LedgerMode::Optional);
        create_tables(&core);
        let first = vec![
            "BEGIN".to_string(),
            do_lit(DO_WRITE),
            witness(1, "k4"),
            "COMMIT".to_string(),
        ];
        assert_eq!(txn_script(&core, "alice", &first), ["ok"; 4]);
    }
    let core = build(&path, None, None, LedgerMode::Optional);
    assert!(recorded(&core, "alice", "k4"));
    assert!(
        !recorded(&core, "bob", "k4"),
        "other tenant's ledger is separate"
    );
    let resend = vec![
        "BEGIN".to_string(),
        do_lit(DO_WRITE),
        witness(1, "k4"),
        "ROLLBACK".to_string(),
    ];
    assert_eq!(
        txn_script(&core, "alice", &resend),
        ["ok", "ok", "23505", "ok"]
    );
    assert_eq!(count(&core, "alice", "t"), 2);
    // 他テナントは同じ operation_id を独立に使える。
    let other = vec![
        "BEGIN".to_string(),
        do_lit(DO_WRITE),
        witness(1, "k4"),
        "COMMIT".to_string(),
    ];
    assert_eq!(txn_script(&core, "bob", &other), ["ok"; 4]);
    assert_eq!(count(&core, "bob", "t"), 2);
    assert_eq!(count(&core, "alice", "t"), 2, "alice's rows are untouched");
}
