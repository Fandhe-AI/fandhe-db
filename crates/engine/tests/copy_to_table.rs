//! テーブル形 `COPY <table> [(<col>, ...)] TO STDOUT`（Issue #1771・WIRE-22 (5)・SQL-34 の
//! ポインタ）の engine API 層の結合テスト。wire-server の `copy::run` が呼ぶのと同じ入口
//! （`EngineCore::begin_copy`／`begin_copy_in_txn`）を駆動し、括弧形との同値性・行数上限（54000）・
//! テナント境界・fail-closed な拒否を固定する。メッセージ層は `wire22_copy_to_table.rs` の担当。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::result_limits::ResultLimits;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

fn open_engine(name: &str, limit: Option<usize>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    if let Some(n) = limit {
        core = core.with_result_limits(ResultLimits::new(n));
    }
    (core, guard)
}

fn insert(core: &EngineCore, tenant: &str, id: u64) {
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx(tenant),
        &mut session,
        &format!("INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[1.0,0.0]', 'ja')"),
    )
    .expect("insert");
}

fn copy_to(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, String> {
    match core.begin_copy(&ctx(tenant), &SessionState::default(), sql) {
        Ok(CopyPlan::To(_, r, _)) => Ok(r),
        Ok(CopyPlan::From(_)) => panic!("expected TO plan"),
        Err(e) => Err(format!("{} {e}", e.wire_code())),
    }
}

fn ids(r: &QueryResult) -> Vec<u64> {
    r.rows.iter().map(|row| row.id).collect()
}

/// WIRE-22 (5): テーブル形は同じ表への括弧形（全列・列リスト）と同一の列・行を返す。
#[test]
fn table_form_matches_parenthesized_form() {
    let (core, _g) = open_engine("copy-to-table-equiv", None);
    for id in 1..=3 {
        insert(&core, "acme", id);
    }
    let table = copy_to(&core, "acme", "COPY docs TO STDOUT").expect("table form");
    let paren = copy_to(
        &core,
        "acme",
        "COPY (SELECT * FROM docs LIMIT 10000) TO STDOUT",
    )
    .expect("paren form");
    assert_eq!(table, paren);
    assert_eq!(table.rows.len(), 3);

    let table = copy_to(&core, "acme", "COPY docs (lang, id) TO STDOUT").expect("cols");
    let paren = copy_to(
        &core,
        "acme",
        "COPY (SELECT lang, id FROM docs LIMIT 10000) TO STDOUT",
    )
    .expect("paren cols");
    assert_eq!(table, paren);
}

/// WIRE-22 (5)・SQL-34: 上限ちょうどは成功し、超過は黙って切り詰めず 54000。
#[test]
fn row_limit_is_exact_and_overflow_is_54000() {
    let (core, _g) = open_engine("copy-to-table-limit", Some(3));
    for id in 1..=3 {
        insert(&core, "acme", id);
    }
    assert_eq!(
        copy_to(&core, "acme", "COPY docs TO STDOUT")
            .expect("at cap")
            .rows
            .len(),
        3
    );
    insert(&core, "acme", 4);
    let err = copy_to(&core, "acme", "COPY docs TO STDOUT").expect_err("over cap");
    assert!(err.starts_with("54000"), "{err}");
    assert!(!err.contains("acme"), "{err}");
}

/// 設定値がハード上限（100_000）のときも COPY は設定値そのものを上限にする
/// （INSERT ... SELECT 用の「ハード上限 - 1」丸めを使わない。束縛が超過検出用の +1 行を
/// 拒否せず、通常の少量行が成功することを固定する）。
#[test]
fn hard_cap_setting_is_not_rounded_down() {
    let (core, _g) = open_engine(
        "copy-to-table-hardcap",
        Some(engine::result_limits::MAX_RESULT_ROWS_HARD_CAP),
    );
    for id in 1..=3 {
        insert(&core, "acme", id);
    }
    assert_eq!(
        copy_to(&core, "acme", "COPY docs TO STDOUT")
            .expect("under hard cap")
            .rows
            .len(),
        3
    );
}

/// RLS: 他テナントの行は出力されない。
#[test]
fn other_tenant_rows_are_not_exported() {
    let (core, _g) = open_engine("copy-to-table-rls", None);
    insert(&core, "acme", 1);
    insert(&core, "beta", 2);
    let r = copy_to(&core, "acme", "COPY docs TO STDOUT").expect("ok");
    assert_eq!(ids(&r), vec![1]);
    let r = copy_to(&core, "beta", "COPY docs (id) TO STDOUT").expect("ok");
    assert_eq!(ids(&r), vec![2]);
}

/// fail-closed な拒否と SQLSTATE（不在 42P01・存在しない列 42703・重複列 42701・空列リスト等 42601）。
#[test]
fn invalid_forms_are_rejected() {
    let (core, _g) = open_engine("copy-to-table-reject", None);
    let code = |sql: &str| copy_to(&core, "acme", sql).expect_err(sql)[..5].to_string();
    assert_eq!(code("COPY nope TO STDOUT"), "42P01");
    assert_eq!(code("COPY docs (nope) TO STDOUT"), "42703");
    assert_eq!(code("COPY docs (lang, lang) TO STDOUT"), "42701");
    assert_eq!(code("COPY docs () TO STDOUT"), "42601");
    assert_eq!(code("COPY docs TO '/tmp/x'"), "42601");
    assert_eq!(code("COPY docs TO STDOUT USING OPERATION_ID 'x'"), "42601");
    // FROM STDIN は列リスト必須のまま（回帰）。
    assert_eq!(code("COPY docs FROM STDIN"), "42601");
}

/// ビューは 42809。
#[test]
fn view_is_wrong_object_type() {
    let (core, _g) = open_engine("copy-to-table-view", None);
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx("acme"), &mut s, "CREATE VIEW v AS SELECT id FROM docs")
        .expect("create view");
    let err = copy_to(&core, "acme", "COPY v TO STDOUT").expect_err("view");
    assert!(err.starts_with("42809"), "{err}");
}

/// 明示トランザクション内（dirty 経路）: 未 commit の行が見え、上限超過は Failed へ遷移する。
#[test]
fn explicit_transaction_sees_uncommitted_rows_and_enforces_cap() {
    let (core, _g) = open_engine("copy-to-table-txn", Some(2));
    let caller = ctx("acme");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    for id in 1..=2 {
        core.execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("INSERT INTO {TABLE} (id, embedding, lang) VALUES ({id}, '[1.0,0.0]', 'ja')"),
        )
        .expect("insert");
    }
    match core
        .begin_copy_in_txn(&caller, &session, &mut txn, "COPY docs TO STDOUT")
        .expect("copy in txn")
    {
        CopyPlan::To(_, r, _) => assert_eq!(ids(&r), vec![1, 2]),
        CopyPlan::From(_) => panic!("expected TO"),
    }
    core.execute_sql_in_txn(
        &caller,
        &mut session,
        &mut txn,
        &format!("INSERT INTO {TABLE} (id, embedding, lang) VALUES (3, '[1.0,0.0]', 'ja')"),
    )
    .expect("insert 3");
    let err = core
        .begin_copy_in_txn(&caller, &session, &mut txn, "COPY docs TO STDOUT")
        .expect_err("over cap");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

/// 一時表（自動コミットと明示 txn）でもテーブル形が効き、上限が同じく適用される。
#[test]
fn temp_table_supports_table_form_and_cap() {
    let (core, _g) = open_engine("copy-to-table-temp", Some(2));
    let caller = ctx("acme");
    let mut session = SessionState::default();
    session.allow_ddl();
    for sql in [
        "CREATE TEMP TABLE tt (id BIGINT PRIMARY KEY, name TEXT)",
        "INSERT INTO tt (id, name) VALUES (1, 'a')",
        "INSERT INTO tt (id, name) VALUES (2, 'b')",
    ] {
        core.execute_sql_in_session(&caller, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    match core
        .begin_copy(&caller, &session, "COPY tt TO STDOUT")
        .expect("auto")
    {
        CopyPlan::To(_, r, _) => assert_eq!(r.rows.len(), 2),
        CopyPlan::From(_) => panic!("expected TO"),
    }
    core.execute_sql_in_session(
        &caller,
        &mut session,
        "INSERT INTO tt (id, name) VALUES (3, 'c')",
    )
    .expect("insert 3");
    let err = core
        .begin_copy(&caller, &session, "COPY tt TO STDOUT")
        .expect_err("over cap (autocommit)");
    assert_eq!(err.wire_code(), "54000");

    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .begin_copy_in_txn(&caller, &session, &mut txn, "COPY tt TO STDOUT")
        .expect_err("over cap (txn)");
    assert_eq!(err.wire_code(), "54000");
}
