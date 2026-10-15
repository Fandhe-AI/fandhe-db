//! `SMALLINT` 列（`ColumnType::SmallInt`。TABLE-30・Issue #1559）の述語・ORDER BY／GROUP BY・
//! 集計・SET 式・二次索引（レーン A。`OrderedColumnIndex::I64`）の結合テスト。
//!
//! 同じ値を持つ INTEGER 列 `m` と Rust 側 i16 オラクルの突き合わせで、二次索引経路と
//! 全走査（`id + 0 > 0` 追加で索引を無効化）の完全一致、負数・境界値・NULL の並び順、
//! 他テナント Private 行の非漏えい（RLS）を固定する。AVG の numeric 化は #2270 の担当で、本テストは現状の公告型（AVG は INTEGER 列と同じ型）を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const ROWS: u64 = 64;
const OTHER_TENANT_ID: u64 = 900;
const POOL: [i16; 8] = [-32768, -32767, -1, 0, 1, 5, 32766, 32767];

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("n", ColumnType::SmallInt, true),
            ColumnDef::new("m", ColumnType::Integer, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `id % 9 == 0` は NULL、それ以外は境界値を含む値プールから決定的に選ぶ。
fn value_of(id: u64) -> Option<i16> {
    if id.is_multiple_of(9) {
        None
    } else {
        Some(POOL[(id % 8) as usize])
    }
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, n: Option<i16>, vis: Visibility) {
    let cell = n
        .map(|v| Value::Integer(i32::from(v)))
        .unwrap_or(Value::Null);
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[Value::Vector(vec![id as f32, 0.0]), cell.clone(), cell],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for id in 1..=ROWS {
        insert_row(&storage, "tenant-a", id, value_of(id), Visibility::Public);
    }
    // 他テナントの Private 行（どの述語にも合う値）。tenant-a からは見えてはならない。
    insert_row(
        &storage,
        "tenant-b",
        OTHER_TENANT_ID,
        Some(5),
        Visibility::Private,
    );
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

fn ids(result: &QueryResult) -> Vec<u64> {
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn oracle(pred: impl Fn(i16) -> bool) -> Vec<u64> {
    (1..=ROWS)
        .filter(|id| value_of(*id).is_some_and(&pred))
        .collect()
}

/// 述語を索引経路（DISTANCE・非 DISTANCE）・全走査強制・INTEGER 列 `m` で実行し、
/// オラクルと一致することを固定する。選択度が十分低い述語では索引が消費されることも確認する。
fn assert_parity(core: &EngineCore, pred_n: &str, pred_m: &str, expected: &[u64]) {
    let vec_sql = format!(
        "SELECT id FROM {TABLE} WHERE {pred_n} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"
    );
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(core, &vec_sql);
    let hot = run(core, &vec_sql);
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(ids(&cold), ids(&hot), "cold/hot mismatch: {pred_n}");
    assert_eq!(ids(&hot), expected, "oracle mismatch: {pred_n}");
    // OR 群（`IN` の脱糖）・`NOT` は索引非対応（全走査で同じ結果になることだけを確認する）。
    let indexable = !pred_n.contains(" IN ") && !pred_n.contains("NOT");
    if indexable && !expected.is_empty() && expected.len() * 2 <= ROWS as usize {
        assert!(after > before, "index not consumed: {pred_n}");
    }
    let scan_sql = format!(
        "SELECT id FROM {TABLE} WHERE {pred_n} AND id + 0 > 0 \
         ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"
    );
    assert_eq!(ids(&run(core, &scan_sql)), expected, "full scan: {pred_n}");
    let plain_sql = format!("SELECT id FROM {TABLE} WHERE {pred_n} LIMIT 200");
    assert_eq!(ids(&run(core, &plain_sql)), expected, "plain: {pred_n}");
    let int_sql = format!("SELECT id FROM {TABLE} WHERE {pred_m} LIMIT 200");
    assert_eq!(
        ids(&run(core, &int_sql)),
        expected,
        "INTEGER parity: {pred_m}"
    );
}

/// `pred` は列名 `n` を使う式。INTEGER 列版は述語中の識別子 `n` だけを `m` に置換する。
fn check(core: &EngineCore, pred: &str, f: impl Fn(i16) -> bool) {
    let m_pred = pred.replace("n ", "m ").replace("NOT n", "NOT m");
    assert_parity(core, pred, &m_pred, &oracle(f));
}

#[test]
fn comparison_predicates_match_index_scan_and_oracle() {
    let (core, _g) = open_core("si-query-cmp");
    check(&core, "n = 5", |v| v == 5);
    check(&core, "n = -32768", |v| v == i16::MIN);
    check(&core, "n = 32767", |v| v == i16::MAX);
    check(&core, "n < 0", |v| v < 0);
    check(&core, "n <= -1", |v| v <= -1);
    check(&core, "n > 1", |v| v > 1);
    check(&core, "n >= 32766", |v| v >= 32766);
    check(&core, "n BETWEEN -1 AND 1", |v| (-1..=1).contains(&v));
    check(&core, "n IN (-32768, 32767)", |v| {
        v == i16::MIN || v == i16::MAX
    });
    check(&core, "n NOT IN (0, 5)", |v| v != 0 && v != 5);
    check(&core, "NOT n = 5", |v| v != 5);
    check(&core, "n = '5'", |v| v == 5);
    check(&core, "n BETWEEN '-1' AND '1'", |v| (-1..=1).contains(&v));
    // 範囲外リテラルはエラーにせず比較として評価する（PG と同じ）。
    check(&core, "n = 40000", |_| false);
    check(&core, "n > -40000", |_| true);
    check(&core, "n < 40000", |_| true);
}

#[test]
fn null_predicates_and_string_literal_range() {
    let (core, _g) = open_core("si-query-null");
    let nulls: Vec<u64> = (1..=ROWS).filter(|id| value_of(*id).is_none()).collect();
    let non_nulls = oracle(|_| true);
    assert_eq!(
        ids(&run(
            &core,
            &format!("SELECT id FROM {TABLE} WHERE n IS NULL LIMIT 200")
        )),
        nulls
    );
    assert_eq!(
        ids(&run(
            &core,
            &format!("SELECT id FROM {TABLE} WHERE n IS NOT NULL LIMIT 200")
        )),
        non_nulls
    );
    // 文字列リテラルの範囲外は PG と同じ 22003。
    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            &format!("SELECT id FROM {TABLE} WHERE n = '40000' LIMIT 5"),
        )
        .expect_err("out of range string literal");
    assert_eq!(err.wire_code(), "22003");
}

#[test]
fn explain_uses_secondary_index_for_smallint() {
    let (core, _g) = open_core("si-query-explain");
    let mut session = SessionState::default();
    let sql = format!(
        "EXPLAIN SELECT id FROM {TABLE} WHERE n > 5 ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
    );
    let outcome = core
        .execute_sql_in_session(&ctx("tenant-a"), &mut session, &sql)
        .expect("EXPLAIN must succeed");
    let SqlOutcome::Explain(result) = outcome else {
        panic!("expected Explain");
    };
    let lines: Vec<String> = result
        .rows
        .iter()
        .filter_map(|r| match r.cells.first() {
            Some(Cell::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(
        lines.iter().any(|l| l == "scalar_plan: index_typed_range"),
        "{lines:?}"
    );
}

#[test]
fn tenant_private_rows_never_leak_through_the_index() {
    let (core, _g) = open_core("si-query-rls");
    for w in ["n = 5", "n >= 5", "n BETWEEN 5 AND 5"] {
        for sql in [
            format!(
                "SELECT id FROM {TABLE} WHERE {w} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"
            ),
            format!("SELECT id FROM {TABLE} WHERE {w} AND id + 0 > 0 LIMIT 200"),
        ] {
            let got = ids(&run(&core, &sql));
            assert!(!got.contains(&OTHER_TENANT_ID), "leak via {sql}");
        }
    }
}

fn cell_str(c: Option<&Cell>) -> String {
    match c {
        Some(Cell::SignedInteger(v)) => v.to_string(),
        Some(Cell::Integer(v)) => v.to_string(),
        Some(Cell::Float(v)) => v.to_string(),
        Some(Cell::Null) | None => "NULL".to_string(),
        Some(other) => format!("{other:?}"),
    }
}

fn first_col(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| cell_str(r.cells.first()))
        .collect()
}

#[test]
fn order_by_is_signed_and_nulls_follow_postgres_defaults() {
    let (core, _g) = open_core("si-query-order");
    let mut present: Vec<i16> = (1..=ROWS).filter_map(value_of).collect();
    present.sort_unstable();
    let nulls = (1..=ROWS).filter(|id| value_of(*id).is_none()).count();
    let null_run = || std::iter::repeat_n("NULL".to_string(), nulls);

    let asc = first_col(&run(
        &core,
        &format!("SELECT n FROM {TABLE} ORDER BY n ASC, id ASC LIMIT 200"),
    ));
    let mut want: Vec<String> = present.iter().map(|v| v.to_string()).collect();
    want.extend(null_run());
    assert_eq!(asc, want, "ASC: NULLS LAST by default");

    let desc = first_col(&run(
        &core,
        &format!("SELECT n FROM {TABLE} ORDER BY n DESC, id ASC LIMIT 200"),
    ));
    let mut want: Vec<String> = null_run().collect();
    want.extend(present.iter().rev().map(|v| v.to_string()));
    assert_eq!(desc, want, "DESC: NULLS FIRST by default");

    // NULL の既定位置（ASC は末尾・DESC は先頭）は上で固定済み。
    let nl = first_col(&run(
        &core,
        &format!("SELECT n FROM {TABLE} ORDER BY n ASC, id ASC LIMIT 200"),
    ));
    assert_eq!(nl.last().map(String::as_str), Some("NULL"));
    assert_eq!(nl.first().map(String::as_str), Some("-32768"));
}

#[test]
fn group_by_distinct_and_having() {
    let (core, _g) = open_core("si-query-group");
    let groups = run(
        &core,
        &format!("SELECT n, COUNT(*) FROM {TABLE} GROUP BY n ORDER BY n LIMIT 100"),
    );
    let mut expected: std::collections::BTreeMap<i16, i64> = Default::default();
    for id in 1..=ROWS {
        if let Some(v) = value_of(id) {
            *expected.entry(v).or_default() += 1;
        }
    }
    let got: Vec<(String, String)> = groups
        .rows
        .iter()
        .filter(|r| !matches!(r.cells.first(), Some(Cell::Null)))
        .map(|r| (cell_str(r.cells.first()), cell_str(r.cells.get(1))))
        .collect();
    let want: Vec<(String, String)> = expected
        .iter()
        .map(|(k, c)| (k.to_string(), c.to_string()))
        .collect();
    assert_eq!(got, want);

    let distinct = run(&core, &format!("SELECT DISTINCT n FROM {TABLE} LIMIT 100"));
    assert_eq!(distinct.rows.len(), expected.len() + 1, "values plus NULL");

    let having = run(
        &core,
        &format!("SELECT n, COUNT(*) AS c FROM {TABLE} GROUP BY n HAVING c >= 7 LIMIT 100"),
    );
    // NULL グループ（`id % 9 == 0` の 7 行）も 1 グループとして数える。
    let null_count = (1..=ROWS).filter(|id| value_of(*id).is_none()).count();
    assert_eq!(
        having.rows.len(),
        expected.values().filter(|c| **c >= 7).count() + usize::from(null_count >= 7)
    );
    // MIN(smallint) を HAVING で比較する（負数の境界を含む）。
    let having_min = run(
        &core,
        &format!("SELECT n, MIN(n) AS lo FROM {TABLE} GROUP BY n HAVING lo < 0 LIMIT 100"),
    );
    assert_eq!(
        having_min.rows.len(),
        expected.keys().filter(|k| **k < 0).count()
    );
}

fn computed_types(result: &QueryResult) -> Vec<Option<ColumnType>> {
    result
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { ty, .. } => ty.clone(),
            other => panic!("expected Computed, got {other:?}"),
        })
        .collect()
}

fn first_row(result: &QueryResult) -> Vec<String> {
    result.rows[0]
        .cells
        .iter()
        .map(|c| cell_str(Some(c)))
        .collect()
}

#[test]
fn aggregates_follow_postgres_result_types() {
    let (core, _g) = open_core("si-query-agg");
    let r = run(
        &core,
        &format!("SELECT SUM(n), MIN(n), MAX(n), COUNT(n), COUNT(DISTINCT n) FROM {TABLE}"),
    );
    assert_eq!(
        computed_types(&r),
        vec![
            Some(ColumnType::BigInt),
            Some(ColumnType::SmallInt),
            Some(ColumnType::SmallInt),
            Some(ColumnType::BigInt),
            Some(ColumnType::BigInt),
        ]
    );
    let vals: Vec<i64> = (1..=ROWS).filter_map(value_of).map(i64::from).collect();
    let mut distinct = vals.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        first_row(&r),
        vec![
            vals.iter().sum::<i64>().to_string(),
            "-32768".to_string(),
            "32767".to_string(),
            vals.len().to_string(),
            distinct.len().to_string(),
        ]
    );

    // AVG は INTEGER 列と同じ型・同じ値（numeric 化は #2270 が smallint を含めて担当）。
    let a = run(&core, &format!("SELECT AVG(n), AVG(m) FROM {TABLE}"));
    let t = computed_types(&a);
    assert_eq!(t[0], t[1]);
    let c = first_row(&a);
    assert_eq!(c[0], c[1]);

    // window 集計。
    let w = run(
        &core,
        &format!(
            "SELECT SUM(n) OVER (PARTITION BY n), MIN(n) OVER (ORDER BY n) FROM {TABLE} LIMIT 5"
        ),
    );
    assert_eq!(
        computed_types(&w),
        vec![Some(ColumnType::BigInt), Some(ColumnType::SmallInt)]
    );
}

#[test]
fn set_expr_is_range_checked_without_side_effects() {
    let (core, _g) = open_core("si-query-set");
    let mut session = SessionState::default();
    let mut exec = |sql: &str| core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql);
    // id 7 → POOL[7] = 32767、id 8 は 8 % 8 = 0 → POOL[0] = -32768。
    let e = exec("UPDATE docs SET n = n + 1 WHERE id = 7 USING OPERATION_ID 'u-over'")
        .expect_err("overflow");
    assert_eq!(e.wire_code(), "22003");
    let e = exec("UPDATE docs SET n = n - 1 WHERE id = 8 USING OPERATION_ID 'u-under'")
        .expect_err("underflow");
    assert_eq!(e.wire_code(), "22003");
    exec("UPDATE docs SET n = n + 1 WHERE id = 8 USING OPERATION_ID 'u-ok'").expect("in range");
    let got = run(
        &core,
        &format!("SELECT n FROM {TABLE} WHERE id = 8 LIMIT 1"),
    );
    assert_eq!(first_col(&got), vec!["-32767".to_string()]);
    let got = run(
        &core,
        &format!("SELECT n FROM {TABLE} WHERE id = 7 LIMIT 1"),
    );
    assert_eq!(first_col(&got), vec!["32767".to_string()]);
}

#[test]
fn arithmetic_projection_and_subquery_comparison() {
    let (core, _g) = open_core("si-query-misc");
    let r = run(
        &core,
        &format!("SELECT n + 1 FROM {TABLE} WHERE id = 1 LIMIT 1"),
    );
    // smallint + int4 リテラルは PostgreSQL でも integer。
    assert_eq!(computed_types(&r), vec![Some(ColumnType::Integer)]);
    // smallint 同士は smallint（Issue #1560）。i16 の溢れは 22003（id 7 は 32767）。
    let r = run(
        &core,
        &format!("SELECT n + n FROM {TABLE} WHERE id = 3 LIMIT 1"),
    );
    assert_eq!(computed_types(&r), vec![Some(ColumnType::SmallInt)]);
    let r = run(
        &core,
        &format!("SELECT n + m FROM {TABLE} WHERE id = 3 LIMIT 1"),
    );
    assert_eq!(computed_types(&r), vec![Some(ColumnType::Integer)]);
    let e = core
        .execute_sql(
            &ctx("tenant-a"),
            &format!("SELECT n + n FROM {TABLE} WHERE id = 7 LIMIT 1"),
        )
        .expect_err("smallint overflow");
    assert_eq!(e.wire_code(), "22003");

    let sub = run(
        &core,
        &format!("SELECT id FROM {TABLE} WHERE n = (SELECT MIN(n) FROM {TABLE}) LIMIT 200"),
    );
    assert_eq!(ids(&sub), oracle(|v| v == i16::MIN));
}

#[test]
fn create_index_on_smallint_is_accepted_and_consumed() {
    let (core, _g) = open_core("si-query-decl-index");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_n ON docs (n)",
    )
    .expect("CREATE INDEX on SMALLINT must be accepted");
    check(&core, "n = 5", |v| v == 5);
    check(&core, "n < 0", |v| v < 0);
}
