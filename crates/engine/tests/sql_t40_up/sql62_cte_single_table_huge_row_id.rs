//! 単一表の具体化 CTE・評価後射影形ビューの外側で `i64::MAX` を超える行 id を評価する
//! 結合テスト（SQL-62・RLS-10 (b)・Issue #2744）。ポインタ: `docs/spec/04-behavior/` 配下の
//! SQL-62・RLS-10。
//!
//! JOIN の入力側は #1861（PR #2729）で numeric(20,0) として束縛済み。単一表の外側
//! （`view_buffered::evaluable_type`）も同じ写像にし、`WHERE`・集計・窓が `22003` にならず、
//! 正しい値を返すこと、Describe と Execute の列が一致すること、範囲外のリテラルは
//! 黙って切り詰めず拒否される（fail-closed）こと、他テナントの行が結果に現れないことを、
//! 本番経路（`EngineCore::execute_sql_in_session`）で確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const HUGE: u64 = i64::MAX as u64 + 7;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, lang: &str, vis: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(format!("row {id}")),
        ],
        &OperationId::parse(&format!("seed-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// alice の public 2 行（id 1 と `i64::MAX` 超）と、bob の非公開行（huge 近傍 id）を持つ表を開く。
fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    insert_row(&storage, "alice", 1, "ja", Visibility::Public);
    insert_row(&storage, "alice", HUGE, "en", Visibility::Public);
    insert_row(&storage, "bob", HUGE + 1, "ja", Visibility::Private);
    insert_row(&storage, "bob", HUGE + 2, "ja", Visibility::Private);
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn ok(core: &EngineCore, sql: &str) -> QueryResult {
    query(core, "alice", sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn exec_ok(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

/// セルを 10 進テキストへ描画する（行 id は `Integer`、numeric 集計は `Numeric`）。
fn text(cell: &Cell) -> String {
    match cell {
        Cell::Integer(v) => v.to_string(),
        Cell::Numeric(d) => d.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Null => "NULL".to_string(),
        other => panic!("unexpected cell {other:?}"),
    }
}

/// 先頭列を数値順に整列したテキスト列として返す。
fn first_col(r: &QueryResult) -> Vec<String> {
    let mut out: Vec<String> = r.rows.iter().map(|row| text(&row.cells[0])).collect();
    out.sort_by_key(|s| s.parse::<u128>().unwrap_or(0));
    out
}

fn huge() -> String {
    HUGE.to_string()
}

const TAIL_CTE: &str = "WITH c AS (SELECT id, lang FROM docs ORDER BY id LIMIT 10) ";
const AGG_CTE: &str = "WITH c AS (SELECT id, count(*) AS n FROM docs GROUP BY id) ";

/// 外側の述語・窓が `i64::MAX` 超の行 id を `22003` にせず評価する。
#[test]
fn tail_cte_outer_predicates_accept_huge_ids() {
    let (core, _g) = open_core("sql62-single-huge-pred");
    let h = huge();
    let r = ok(&core, &format!("{TAIL_CTE}SELECT * FROM c WHERE id > 0"));
    assert_eq!(r.columns[0], ColumnMeta::Id);
    assert_eq!(first_col(&r), vec!["1".to_string(), h.clone()]);
    for pred in [
        format!("id = {h}"),
        "id > 9223372036854775807".to_string(),
        format!("id IN ({h})"),
        format!("id BETWEEN 9223372036854775808 AND {h}"),
    ] {
        let r = ok(&core, &format!("{TAIL_CTE}SELECT id FROM c WHERE {pred}"));
        assert_eq!(first_col(&r), vec![h.clone()], "{pred}");
    }
    let r = ok(
        &core,
        &format!("{TAIL_CTE}SELECT id, row_number() OVER (ORDER BY id) AS rn FROM c"),
    );
    let ids: Vec<String> = r.rows.iter().map(|row| text(&row.cells[0])).collect();
    assert_eq!(ids, vec!["1".to_string(), h]);
}

/// 外側の集計・`GROUP BY id` も numeric で評価され、値が一致する。
#[test]
fn tail_cte_outer_aggregates_accept_huge_ids() {
    let (core, _g) = open_core("sql62-single-huge-agg");
    let h = huge();
    let r = ok(&core, &format!("{TAIL_CTE}SELECT MAX(id) FROM c"));
    assert_eq!(first_col(&r), vec![h.clone()]);
    let r = ok(&core, &format!("{TAIL_CTE}SELECT MIN(id) FROM c"));
    assert_eq!(first_col(&r), vec!["1".to_string()]);
    let r = ok(
        &core,
        &format!("{TAIL_CTE}SELECT COUNT(*) FROM c WHERE id > 9223372036854775807"),
    );
    assert_eq!(first_col(&r), vec!["1".to_string()]);
    let r = ok(
        &core,
        &format!("{TAIL_CTE}SELECT id, COUNT(*) FROM c GROUP BY id"),
    );
    assert_eq!(first_col(&r), vec!["1".to_string(), h]);
}

/// 本文が集計の CTE でも、外側述語は `22003` にならない。
#[test]
fn aggregate_cte_outer_predicate_accepts_huge_ids() {
    let (core, _g) = open_core("sql62-single-huge-aggcte");
    let r = ok(&core, &format!("{AGG_CTE}SELECT id FROM c WHERE id > 0"));
    assert_eq!(first_col(&r), vec!["1".to_string(), huge()]);
    let r = ok(
        &core,
        &format!("{AGG_CTE}SELECT id FROM c WHERE id = {}", huge()),
    );
    assert_eq!(first_col(&r), vec![huge()]);
}

/// 評価後射影形ビューの外側でも同じ形が通る。
#[test]
fn view_outer_forms_accept_huge_ids() {
    let (core, _g) = open_core("sql62-single-huge-view");
    exec_ok(
        &core,
        "CREATE VIEW v AS SELECT id, lang FROM docs ORDER BY id LIMIT 5",
    );
    let h = huge();
    let r = ok(&core, "SELECT * FROM v WHERE id > 0");
    assert_eq!(first_col(&r), vec!["1".to_string(), h.clone()]);
    let r = ok(&core, &format!("SELECT id FROM v WHERE id = {h}"));
    assert_eq!(first_col(&r), vec![h.clone()]);
    let r = ok(&core, "SELECT MAX(id) FROM v");
    assert_eq!(first_col(&r), vec![h.clone()]);
    let r = ok(&core, "SELECT id, COUNT(*) FROM v GROUP BY id");
    assert_eq!(first_col(&r), vec!["1".to_string(), h]);
}

/// 対照: 列名リスト付き CTE（従来から numeric 経路）は前後で通る。
#[test]
fn column_list_cte_control_still_works() {
    let (core, _g) = open_core("sql62-single-huge-control");
    let r = ok(
        &core,
        "WITH c(i) AS (SELECT id FROM docs ORDER BY id LIMIT 10) SELECT i FROM c WHERE i > 0",
    );
    assert_eq!(first_col(&r), vec!["1".to_string(), huge()]);
}

/// Describe の列は Execute の列と一致する。
#[test]
fn describe_matches_execute() {
    let (core, _g) = open_core("sql62-single-huge-describe");
    for sql in [
        format!("{TAIL_CTE}SELECT * FROM c WHERE id > 0"),
        format!("{TAIL_CTE}SELECT MAX(id) AS m FROM c"),
        format!("{TAIL_CTE}SELECT id, COUNT(*) AS n FROM c GROUP BY id"),
        format!("{TAIL_CTE}SELECT id, row_number() OVER (ORDER BY id) AS rn FROM c"),
    ] {
        let executed = ok(&core, &sql);
        let session = SessionState::default();
        let parsed = core.parse_sql(&sql).expect("parse");
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("has columns");
        assert_eq!(described, executed.columns, "{sql}");
    }
}

/// fail-closed: u64 範囲外のリテラルは黙って切り詰めず、どの行にも一致しない。
/// 他テナントの行（非公開含む）は件数に現れない（RLS-10 (b)）。
#[test]
fn out_of_range_literal_never_truncates_and_tenants_stay_isolated() {
    let (core, _g) = open_core("sql62-single-huge-failclosed");
    // u64 を超えるリテラルは numeric として比較され、どの行にも一致しない（切り詰めて huge 行に
    // 一致させない）。
    let r = ok(
        &core,
        &format!("{TAIL_CTE}SELECT id FROM c WHERE id = 18446744073709551616000"),
    );
    assert!(r.rows.is_empty());
    let r = ok(
        &core,
        &format!("{TAIL_CTE}SELECT COUNT(*) FROM c WHERE id > 0"),
    );
    assert_eq!(first_col(&r), vec!["2".to_string()]);
}
