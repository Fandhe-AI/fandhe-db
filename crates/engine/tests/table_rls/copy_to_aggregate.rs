//! `COPY (<集計 SELECT>) TO STDOUT`（Issue #3253・SQL-34・WIRE-22 のポインタ）の engine API 層の
//! 結合テスト。wire-server の `copy::run` が呼ぶのと同じ入口（`EngineCore::begin_copy`／
//! `begin_copy_in_txn`）を駆動し、通常の SELECT との同値性・行数上限（54000）・テナント境界・
//! 範囲外形の fail-closed な拒否を固定する。メッセージ層は wire-server 側の担当。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::result_limits::ResultLimits;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
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

fn insert(core: &EngineCore, tenant: &str, id: u64, lang: &str) {
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx(tenant),
        &mut session,
        &format!("INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[1.0,0.0]', '{lang}')"),
    )
    .expect("insert");
}

/// acme へ 3 言語 6 行（ja×3・en×2・fr×1）、beta へ 2 行入れる。
fn seed(core: &EngineCore) {
    for (id, lang) in [
        (1, "ja"),
        (2, "ja"),
        (3, "ja"),
        (4, "en"),
        (5, "en"),
        (6, "fr"),
    ] {
        insert(core, "acme", id, lang);
    }
    insert(core, "beta", 100, "ja");
    insert(core, "beta", 101, "de");
}

fn copy_to(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, String> {
    match core.begin_copy(&ctx(tenant), &SessionState::default(), sql) {
        Ok(CopyPlan::To(_, r, _)) => Ok(r),
        Ok(CopyPlan::From(_)) => panic!("expected TO plan"),
        Err(e) => Err(format!("{} {e}", e.wire_code())),
    }
}

fn select(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
    {
        SqlOutcome::Query(r) => r,
        _ => panic!("expected query outcome"),
    }
}

/// 内側 SELECT を直接実行した結果と COPY の結果が（列メタ・行とも）一致する。
fn assert_same_as_select(core: &EngineCore, inner: &str) {
    for suffix in ["", ";"] {
        let copy = copy_to(core, "acme", &format!("COPY ({inner}) TO STDOUT{suffix}"))
            .unwrap_or_else(|e| panic!("{inner}: {e}"));
        assert_eq!(copy, select(core, "acme", inner), "{inner}");
    }
}

#[test]
fn aggregate_matches_plain_select() {
    let (core, _g) = open_engine("copy-agg-equiv", None);
    seed(&core);
    for inner in [
        "SELECT count(*) FROM docs",
        "SELECT lang, count(*) FROM docs GROUP BY lang ORDER BY lang",
        "SELECT lang, count(*) FROM docs GROUP BY lang HAVING count(*) > 1 ORDER BY lang",
        "SELECT DISTINCT lang FROM docs ORDER BY lang",
        "SELECT lang, count(*) FROM docs GROUP BY ROLLUP (lang) ORDER BY lang",
        "SELECT count(*) FILTER (WHERE lang = 'ja') FROM docs",
        // 評価後射影を要しない CTE は通常の SELECT と同じ集計へ畳まれる（同値であることを固定）。
        "WITH c AS (SELECT id FROM docs) SELECT count(*) FROM c",
    ] {
        assert_same_as_select(&core, inner);
    }
    let r = copy_to(&core, "acme", "COPY (SELECT count(*) FROM docs) TO STDOUT").expect("count");
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn tenant_boundary_is_kept() {
    let (core, _g) = open_engine("copy-agg-tenant", None);
    seed(&core);
    let r = copy_to(
        &core,
        "acme",
        "COPY (SELECT lang, count(*) FROM docs GROUP BY lang ORDER BY lang) TO STDOUT",
    )
    .expect("grouped");
    // acme は en/fr/ja の 3 グループのみ（beta の de は現れない）。
    assert_eq!(r.rows.len(), 3);
    let r = copy_to(&core, "beta", "COPY (SELECT count(*) FROM docs) TO STDOUT").expect("beta");
    assert_eq!(r, select(&core, "beta", "SELECT count(*) FROM docs"));
}

#[test]
fn row_limit_is_exact_and_overflow_is_54000() {
    let (core, _g) = open_engine("copy-agg-limit", Some(3));
    seed(&core);
    let grouped = "COPY (SELECT lang, count(*) FROM docs GROUP BY lang) TO STDOUT";
    assert_eq!(
        copy_to(&core, "acme", grouped).expect("at cap").rows.len(),
        3
    );
    insert(&core, "acme", 7, "zh");
    let err = copy_to(&core, "acme", grouped).expect_err("over cap");
    assert!(err.starts_with("54000"), "{err}");
    assert!(!err.contains("acme"), "{err}");
    // 明示 LIMIT が上限以下なら成功する。
    let ok = copy_to(
        &core,
        "acme",
        "COPY (SELECT lang, count(*) FROM docs GROUP BY lang ORDER BY lang LIMIT 2) TO STDOUT",
    )
    .expect("limited");
    assert_eq!(ok.rows.len(), 2);
    // 単一行集計は上限が小さくても成功する。
    let (core1, _g1) = open_engine("copy-agg-limit1", Some(1));
    seed(&core1);
    assert_eq!(
        copy_to(&core1, "acme", "COPY (SELECT count(*) FROM docs) TO STDOUT")
            .expect("single")
            .rows
            .len(),
        1
    );
}

#[test]
fn explicit_transaction_sees_uncommitted_rows() {
    let (core, _g) = open_engine("copy-agg-txn", None);
    let caller = ctx("acme");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &caller,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja')",
    )
    .expect("insert");
    let expected = match core
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "SELECT count(*) FROM docs")
        .expect("select")
    {
        SqlOutcome::Query(r) => r,
        _ => panic!("expected query"),
    };
    match core
        .begin_copy_in_txn(
            &caller,
            &session,
            &mut txn,
            "COPY (SELECT count(*) FROM docs) TO STDOUT",
        )
        .expect("copy in txn")
    {
        CopyPlan::To(_, r, _) => assert_eq!(r, expected),
        CopyPlan::From(_) => panic!("expected TO"),
    }
}

#[test]
fn temp_table_aggregate() {
    let (core, _g) = open_engine("copy-agg-temp", None);
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
    let expected = match core
        .execute_sql_in_session(&caller, &mut session, "SELECT count(*) FROM tt")
        .expect("select")
    {
        SqlOutcome::Query(r) => r,
        _ => panic!("expected query"),
    };
    match core
        .begin_copy(
            &caller,
            &session,
            "COPY (SELECT count(*) FROM tt) TO STDOUT",
        )
        .expect("copy")
    {
        CopyPlan::To(_, r, _) => assert_eq!(r, expected),
        CopyPlan::From(_) => panic!("expected TO"),
    }
}

#[test]
fn out_of_scope_shapes_stay_rejected() {
    let (core, _g) = open_engine("copy-agg-reject", None);
    seed(&core);
    for (inner, codes) in [
        (
            "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3",
            &["42601"][..],
        ),
        ("SELECT 1", &["42601"][..]),
        (
            "SELECT count(*) FROM docs a JOIN docs b ON a.id = b.id",
            &["42601", "0A000"][..],
        ),
        (
            "SELECT id FROM docs UNION SELECT id FROM docs",
            &["42601", "0A000"][..],
        ),
        // WHERE 中のサブクエリは COPY の構文検証経路では未対応（Scan 形と同じ。fail-closed）。
        (
            "SELECT count(*) FROM docs WHERE lang IN (SELECT lang FROM docs WHERE id > 3)",
            &["42601"][..],
        ),
        ("SELECT count(*) FROM nope", &["42P01"][..]),
    ] {
        let err = copy_to(&core, "acme", &format!("COPY ({inner}) TO STDOUT")).expect_err(inner);
        assert!(
            codes.iter().any(|c| err.starts_with(c)),
            "{inner}: unexpected {err}"
        );
    }
}
