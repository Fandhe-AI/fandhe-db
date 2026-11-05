//! `INSERT` の `EXPLAIN` 結合テスト（Issue #2051・SQL-75 (2)(3)(5)(6) ポインタ）。
//!
//! 文を実行せず、挿入方式の段の木（`insert`／`select`／`result`）を返す。書き込み・台帳・採番を起こさず、
//! 出力は閉じた語彙だけでテナント・行数・名前に依存しない。束縛エラーは実行したときと同じ `wire_code`。
//! 未対応の形（ビュー・`$n`・セッションの無い入口）は fail-closed で拒否する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionLimits};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("setup should succeed: sql={sql:?} err={e:?}"))
}

/// t(id PK, k, v)・src(id PK, k, v)・d（DEFAULT now() を持つ表）。tenant-a にだけ行がある。
fn seeded(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k BIGINT, v TEXT)",
        "CREATE TABLE src (id BIGINT PRIMARY KEY, k BIGINT, v TEXT)",
        "CREATE TABLE u (id BIGINT PRIMARY KEY, k BIGINT UNIQUE, v TEXT)",
        "INSERT INTO src (id, k, v) VALUES (1, 10, 'a'), (2, 20, 'b')",
        "INSERT INTO t (id, k, v) VALUES (100, 1, 'seed')",
    ] {
        exec_as(&core, "tenant-a", sql);
    }
    (core, guard)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn lines_of(outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Explain(r) => r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn plan_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    lines_of(run(core, tenant, sql).unwrap_or_else(|c| panic!("{sql} failed with {c}")))
}

fn plan(core: &EngineCore, sql: &str) -> Vec<String> {
    plan_as(core, "tenant-a", sql)
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, "tenant-a", sql) {
        Err(c) => c,
        Ok(_) => panic!("{sql} must be rejected"),
    }
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn count(core: &EngineCore, table: &str) -> usize {
    match run(
        core,
        "tenant-a",
        &format!("SELECT id FROM {table} LIMIT 100"),
    )
    .expect("select")
    {
        SqlOutcome::Query(r) => r.rows.len(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn single_row_values_renders_fixed_vocabulary() {
    let (core, _g) = seeded("x75i-row");
    let got = plan(
        &core,
        "EXPLAIN INSERT INTO t (id, k, v) VALUES (1, 2, 'x') USING OPERATION_ID 'op-1'",
    );
    assert_eq!(
        got,
        strs(&[
            "insert",
            "insert_source: values",
            "insert_method: row",
            "on_conflict: none",
            "returning: none",
            "operation_id: present",
            "-> result",
            "   source: values",
        ])
    );
}

#[test]
fn multi_row_and_default_values_and_returning() {
    let (core, _g) = seeded("x75i-forms");
    let got = plan(
        &core,
        "EXPLAIN INSERT INTO t (id, k, v) VALUES (1, 2, 'x'), (2, 3, 'y') RETURNING id",
    );
    assert!(
        got.contains(&"insert_method: row_batch".to_string()),
        "{got:?}"
    );
    assert!(got.contains(&"returning: present".to_string()), "{got:?}");
    assert!(got.contains(&"operation_id: none".to_string()), "{got:?}");
    let got = plan(&core, "EXPLAIN INSERT INTO t DEFAULT VALUES");
    assert!(
        got.contains(&"insert_source: default_values".to_string()),
        "{got:?}"
    );
}

#[test]
fn on_conflict_reports_target_kind_and_filter() {
    let (core, _g) = seeded("x75i-conflict");
    let got = plan(
        &core,
        "EXPLAIN INSERT INTO u (id, k, v) VALUES (1, 2, 'x') ON CONFLICT (id) DO NOTHING",
    );
    assert!(
        got.contains(&"insert_method: upsert".to_string()),
        "{got:?}"
    );
    assert!(
        got.contains(&"on_conflict: do_nothing".to_string()),
        "{got:?}"
    );
    assert!(
        got.contains(&"conflict_target: unique".to_string()),
        "{got:?}"
    );
    assert!(
        !got.iter().any(|l| l.starts_with("conflict_filter")),
        "{got:?}"
    );

    let got = plan(
        &core,
        "EXPLAIN INSERT INTO u (id, k, v) VALUES (1, 2, 'x') ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v WHERE u.v <> 'z'",
    );
    assert!(
        got.contains(&"on_conflict: do_update".to_string()),
        "{got:?}"
    );
    assert!(
        got.contains(&"conflict_target: unique".to_string()),
        "{got:?}"
    );
    assert!(
        got.contains(&"conflict_filter: present".to_string()),
        "{got:?}"
    );

    let got = plan(
        &core,
        "EXPLAIN INSERT INTO u (id, k, v) VALUES (1, 2, 'x') ON CONFLICT DO NOTHING",
    );
    assert!(
        got.contains(&"conflict_target: omitted".to_string()),
        "{got:?}"
    );
}

#[test]
fn insert_select_nests_inner_plan_under_select_stage() {
    let (core, _g) = seeded("x75i-select");
    let got = plan(
        &core,
        "EXPLAIN INSERT INTO t (id, k, v) SELECT id, k, v FROM src",
    );
    assert_eq!(got.first().map(String::as_str), Some("insert"));
    assert!(
        got.contains(&"insert_source: select".to_string()),
        "{got:?}"
    );
    assert!(
        !got.iter().any(|l| l.starts_with("insert_method")),
        "{got:?}"
    );
    assert!(got.contains(&"-> select".to_string()), "{got:?}");
    assert!(got.iter().any(|l| l.contains("scalar_plan:")), "{got:?}");
    // 表名・列名・行数は出さない。
    for l in &got {
        assert!(!l.contains("src") && !l.contains("seed"), "{l}");
    }

    let got = plan(&core, "EXPLAIN INSERT INTO t (id, k, v) SELECT 9, 9, 'c'");
    assert!(
        got.contains(&"source: constant".to_string())
            || got.iter().any(|l| l.ends_with("source: constant")),
        "{got:?}"
    );
}

#[test]
fn explain_writes_nothing_and_does_not_record_ledger() {
    let (core, _g) = seeded("x75i-no-write");
    let before = count(&core, "t");
    plan(
        &core,
        "EXPLAIN INSERT INTO t (id, k, v) VALUES (1, 2, 'x') USING OPERATION_ID 'op-x'",
    );
    plan(
        &core,
        "EXPLAIN INSERT INTO t (id, k, v) SELECT id, k, v FROM src USING OPERATION_ID 'op-y'",
    );
    assert_eq!(count(&core, "t"), before);
    // 同じ operation_id の本物の INSERT は新規として書き込まれる（台帳に記録していない）。
    assert!(matches!(
        run(
            &core,
            "tenant-a",
            "INSERT INTO t (id, k, v) VALUES (1, 2, 'x') USING OPERATION_ID 'op-x'"
        ),
        Ok(SqlOutcome::Insert(_))
    ));
    assert_eq!(count(&core, "t"), before + 1);
}

#[test]
fn binding_errors_match_the_plain_statement() {
    let (core, _g) = seeded("x75i-parity");
    for sql in [
        "INSERT INTO nope (id) VALUES (1)",
        "INSERT INTO t (id, nope) VALUES (1, 2)",
        "INSERT INTO t (id, k) VALUES (1, 'not-a-number')",
        "INSERT INTO t (id, k, v) VALUES (1, 2)",
        "INSERT INTO t (id, k, v) VALUES (1, 2, 'x') ON CONFLICT (v) DO NOTHING",
        "INSERT INTO t (id, k, v) SELECT id, k FROM src",
        "INSERT INTO t (id, k, v) SELECT id, v, v FROM src",
        "INSERT INTO t (id, k, v) SELECT id, k, v FROM nope",
    ] {
        let want = code(&core, sql);
        let got = code(&core, &format!("EXPLAIN {sql}"));
        assert_eq!(got, want, "{sql}");
    }
}

#[test]
fn output_is_identical_across_tenants() {
    let (core, _g) = seeded("x75i-tenant");
    // tenant-a だけが行を持つ。可視行数の異なる tenant 間でも出力はバイト一致する。
    let sql = "EXPLAIN INSERT INTO t (id, k, v) VALUES (1, 2, 'x')";
    let a = plan_as(&core, "tenant-a", sql);
    let b = plan_as(&core, "tenant-b", sql);
    assert_eq!(a, b);
    exec_as(
        &core,
        "tenant-a",
        "INSERT INTO t (id, k, v) VALUES (200, 1, 'more'), (201, 2, 'more')",
    );
    assert_eq!(plan_as(&core, "tenant-a", sql), b);
}

#[test]
fn unsupported_forms_are_rejected_fail_closed() {
    let (core, _g) = seeded("x75i-reject");
    exec_as(
        &core,
        "tenant-a",
        "CREATE VIEW vw AS SELECT id, k, v FROM t",
    );
    assert_eq!(
        code(
            &core,
            "EXPLAIN INSERT INTO vw (id, k, v) VALUES (1, 2, 'x')"
        ),
        "0A000"
    );
    // セッションの無い入口は従来どおり `42601`。
    let e = core
        .execute_sql(
            &ctx("tenant-a"),
            "EXPLAIN INSERT INTO t (id, k, v) VALUES (1, 2, 'x')",
        )
        .expect_err("no-session entry rejects");
    assert_eq!(e.wire_code(), "42601");
}

#[test]
fn works_inside_transactions_including_read_only() {
    let (core, _g) = seeded("x75i-txn");
    let c = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = SessionTransaction::new(TransactionLimits::default());
    let mut exec = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    exec(&mut s, "BEGIN");
    let out = exec(
        &mut s,
        "EXPLAIN INSERT INTO t (id, k, v) VALUES (7, 7, 'x') USING OPERATION_ID 'op-tx'",
    );
    assert!(matches!(out, SqlOutcome::Explain(_)));
    // EXPLAIN は operation_id を登録しないので、同じ ID の本物の INSERT が通る。
    exec(
        &mut s,
        "INSERT INTO t (id, k, v) VALUES (7, 7, 'x') USING OPERATION_ID 'op-tx'",
    );
    exec(&mut s, "COMMIT");
    exec(&mut s, "BEGIN READ ONLY");
    let out = exec(
        &mut s,
        "EXPLAIN INSERT INTO t (id, k, v) VALUES (8, 8, 'x')",
    );
    assert!(matches!(out, SqlOutcome::Explain(_)));
    exec(&mut s, "ROLLBACK");
}

/// INSERT 権限の無いロールは `EXPLAIN INSERT` も `42501`（通常の INSERT と同じビットを課す）。
/// INSERT だけ持つロールの VALUES は成功し、元表に SELECT が無い `INSERT ... SELECT` は `42501`。
#[test]
fn explain_requires_the_same_privileges_as_the_plain_insert() {
    let (core, _g) = seeded("x75i-priv");
    let c = ctx("tenant-a");
    let mut op = SessionState::default();
    op.allow_ddl();
    for sql in ["CREATE ROLE alice", "CREATE ROLE bob"] {
        core.execute_sql_in_session(&c, &mut op, sql).expect(sql);
    }
    let mut alice = SessionState::default();
    alice.bind_role_ids(1, 1);
    alice.allow_ddl();
    for sql in [
        "CREATE TABLE p (id BIGINT PRIMARY KEY, v TEXT)",
        "CREATE TABLE ps (id BIGINT PRIMARY KEY, v TEXT)",
    ] {
        core.execute_sql_in_session(&c, &mut alice, sql).expect(sql);
    }
    let mut bob = SessionState::default();
    bob.bind_role_ids(2, 2);
    let sql_values = "EXPLAIN INSERT INTO p (id, v) VALUES (1, 'x')";
    let sql_select = "EXPLAIN INSERT INTO p (id, v) SELECT id, v FROM ps";
    let denied = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_session(&c, s, sql)
            .expect_err(sql)
            .wire_code()
    };
    assert_eq!(denied(&mut bob, sql_values), "42501");
    core.execute_sql_in_session(&c, &mut alice, "GRANT INSERT ON p TO bob")
        .expect("grant");
    assert!(core
        .execute_sql_in_session(&c, &mut bob, sql_values)
        .is_ok());
    assert_eq!(denied(&mut bob, sql_select), "42501");
}

/// 内側 kNN が `LIMIT` 省略（構文段が cap+1 を注入する形）のとき、設定上限がハード上限未満でも
/// 通常の `INSERT ... SELECT` が受理される文は EXPLAIN でも受理される（受理条件の一致）。
#[test]
fn insert_select_with_injected_limit_is_explained_under_lowered_result_limits() {
    let path = unique_db_path("x75i-injected-limit");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(engine::result_limits::ResultLimits::new(10));
    for sql in [
        "CREATE TABLE kv (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL)",
        "CREATE TABLE kd (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL)",
        "INSERT INTO kv (id, embedding) VALUES (1, '[1,0]'), (2, '[0,1]')",
    ] {
        exec_as(&core, "tenant-a", sql);
    }
    let inner = "INSERT INTO kd (id, embedding) SELECT id, embedding FROM kv \
                 ORDER BY embedding <=> '[1,0]'";
    // 通常実行が受理されることを前提にする。
    run(&core, "tenant-a", inner).expect("plain INSERT ... SELECT is accepted");
    let got = plan(&core, &format!("EXPLAIN {inner}"));
    assert_eq!(got.first().map(String::as_str), Some("insert"), "{got:?}");
    assert!(got.contains(&"-> select".to_string()), "{got:?}");
}
