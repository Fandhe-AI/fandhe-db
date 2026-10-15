//! `TIME` 列（`ColumnType::Time`。TABLE-32・Issue #1570）の述語・ORDER BY／GROUP BY・集計・
//! 二次索引（レーン B。`OrderedColumnIndex::I64`）・`EXTRACT`／`date_part`・サブクエリ・集合演算・
//! SET 式の結合テスト。
//!
//! 同じ値を 0 時起点のマイクロ秒で持つ BIGINT 列 `m` と Rust 側 i64 オラクルの突き合わせで、
//! 二次索引経路・全走査（`id + 0 > 0` 追加で索引を無効化）・オラクルの 3 者が完全一致すること
//! （受け入れ条件）、`24:00:00`・`00:00:00.000001` 等の境界値、NULL の並び順、他テナント Private 行の
//! 非漏えい（RLS）を固定する。PK/UNIQUE・型変更・算術・TIMESTAMP／DATE との暗黙比較は
//! fail-closed のまま（担当 Issue 未定）であることも固定する。

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
const H: i64 = 3_600_000_000;
const M: i64 = 60_000_000;
const S: i64 = 1_000_000;
const DAY: i64 = 24 * H;
/// 境界値を含む値プール（マイクロ秒）。
const POOL: [i64; 8] = [
    0,
    1,
    5 * H + 6 * M + 7 * S,
    12 * H,
    12 * H + 34 * M + 56 * S + 789_012,
    DAY - 1,
    DAY,
    6 * H,
];

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("v", ColumnType::Time, true),
            ColumnDef::new("m", ColumnType::BigInt, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `id % 9 == 0` は NULL、それ以外は境界値を含む値プールから決定的に選ぶ。
fn value_of(id: u64) -> Option<i64> {
    if id.is_multiple_of(9) {
        None
    } else {
        Some(POOL[(id % 8) as usize])
    }
}

fn lit(micros: i64) -> String {
    engine::datetime::format_time(micros)
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, v: Option<i64>, vis: Visibility) {
    let t = v.map(Value::Time).unwrap_or(Value::Null);
    let m = v.map(Value::BigInt).unwrap_or(Value::Null);
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[Value::Vector(vec![id as f32, 0.0]), t, m],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn open_core(label: &str) -> (EngineCore, CleanupGuard) {
    let (core, guard, _p) = open_core_with_path(label);
    (core, guard)
}

fn open_core_with_path(label: &str) -> (EngineCore, CleanupGuard, std::path::PathBuf) {
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
        Some(12 * H),
        Visibility::Private,
    );
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
        path,
    )
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"))
}

fn code(core: &EngineCore, sql: &str) -> String {
    core.execute_sql(&ctx("tenant-a"), sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

fn ids(result: &QueryResult) -> Vec<u64> {
    let mut v: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    v.sort_unstable();
    v
}

fn oracle(pred: impl Fn(i64) -> bool) -> Vec<u64> {
    (1..=ROWS)
        .filter(|id| value_of(*id).is_some_and(&pred))
        .collect()
}

/// 述語を索引経路（DISTANCE・非 DISTANCE）・全走査強制・BIGINT 列 `m` で実行し、
/// オラクルと完全一致することを固定する（受け入れ条件: 索引あり／なしで結果が一致）。
/// `expect_index` が真で選択度が十分低い述語では索引が消費されることも確認する。
fn assert_parity(
    core: &EngineCore,
    pred_v: &str,
    pred_m: &str,
    expected: &[u64],
    expect_index: bool,
) {
    let vec_sql = format!(
        "SELECT id FROM {TABLE} WHERE {pred_v} ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"
    );
    let before = core.scalar_index_cache_stats().index_scans;
    let cold = run(core, &vec_sql);
    let hot = run(core, &vec_sql);
    let after = core.scalar_index_cache_stats().index_scans;
    assert_eq!(ids(&cold), ids(&hot), "cold/hot mismatch: {pred_v}");
    assert_eq!(ids(&hot), expected, "oracle mismatch: {pred_v}");
    if expect_index && !expected.is_empty() && expected.len() * 2 <= ROWS as usize {
        assert!(after > before, "index not consumed: {pred_v}");
    }
    let scan_sql = format!(
        "SELECT id FROM {TABLE} WHERE {pred_v} AND id + 0 > 0 \
         ORDER BY embedding <=> '[1.0,0.0]' LIMIT 200"
    );
    assert_eq!(ids(&run(core, &scan_sql)), expected, "full scan: {pred_v}");
    let plain_sql = format!("SELECT id FROM {TABLE} WHERE {pred_v} LIMIT 200");
    assert_eq!(ids(&run(core, &plain_sql)), expected, "plain: {pred_v}");
    let int_sql = format!("SELECT id FROM {TABLE} WHERE {pred_m} LIMIT 200");
    assert_eq!(
        ids(&run(core, &int_sql)),
        expected,
        "BIGINT parity: {pred_m}"
    );
}

/// `op`（`=`・`<` 等）と境界値リテラルから TIME 版・BIGINT 版の述語を作って照合する。
fn check_cmp(core: &EngineCore, op: &str, micros: i64, f: impl Fn(i64, i64) -> bool) {
    let pv = format!("v {op} '{}'", lit(micros));
    let pm = format!("m {op} {micros}");
    assert_parity(core, &pv, &pm, &oracle(|x| f(x, micros)), true);
}

fn all_checks(core: &EngineCore) {
    for micros in [0, 1, 6 * H, 12 * H, DAY - 1, DAY] {
        check_cmp(core, "=", micros, |a, b| a == b);
        check_cmp(core, "<", micros, |a, b| a < b);
        check_cmp(core, "<=", micros, |a, b| a <= b);
        check_cmp(core, ">", micros, |a, b| a > b);
        check_cmp(core, ">=", micros, |a, b| a >= b);
    }
    // `NOT =` は索引非対応（全走査で同じ結果になることだけを確認する）。
    let pm = format!("NOT m = {}", 12 * H);
    assert_parity(
        core,
        &format!("NOT v = '{}'", lit(12 * H)),
        &pm,
        &oracle(|x| x != 12 * H),
        false,
    );
    assert_parity(
        core,
        &format!("v BETWEEN '{}' AND '{}'", lit(6 * H), lit(DAY - 1)),
        &format!("m BETWEEN {} AND {}", 6 * H, DAY - 1),
        &oracle(|x| (6 * H..=DAY - 1).contains(&x)),
        true,
    );
    assert_parity(
        core,
        &format!("v NOT BETWEEN '{}' AND '{}'", lit(1), lit(12 * H)),
        &format!("m NOT BETWEEN {} AND {}", 1, 12 * H),
        &oracle(|x| !(1..=12 * H).contains(&x)),
        false,
    );
    assert_parity(
        core,
        &format!("v IN ('{}', '{}')", lit(0), lit(DAY)),
        &format!("m IN ({}, {})", 0, DAY),
        &oracle(|x| x == 0 || x == DAY),
        false,
    );
    assert_parity(
        core,
        &format!("v NOT IN ('{}', '{}')", lit(0), lit(DAY)),
        &format!("m NOT IN ({}, {})", 0, DAY),
        &oracle(|x| x != 0 && x != DAY),
        false,
    );
    // `24:00:00` は有効な値で、`'24:00'` の表記でも同じ値になる。
    assert_parity(
        core,
        "v = '24:00'",
        &format!("m = {DAY}"),
        &oracle(|x| x == DAY),
        true,
    );
    assert_parity(
        core,
        "v > '23:59:59.999999'",
        &format!("m > {}", DAY - 1),
        &oracle(|x| x == DAY),
        true,
    );
}

#[test]
fn comparison_predicates_match_index_scan_and_oracle() {
    let (core, _g) = open_core("tq-cmp");
    all_checks(&core);
}

#[test]
fn declared_index_gives_the_same_results_as_the_automatic_one() {
    let (core, _g) = open_core("tq-decl-index");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "CREATE INDEX idx_v ON docs (v)",
    )
    .expect("CREATE INDEX on TIME must be accepted");
    all_checks(&core);
}

#[test]
fn typed_time_literal_comparisons_match_the_oracle() {
    let (core, _g) = open_core("tq-typed-literal");
    for (op, f) in [
        ("=", (|a: i64, b: i64| a == b) as fn(i64, i64) -> bool),
        ("<", |a, b| a < b),
        (">=", |a, b| a >= b),
    ] {
        let pred = format!("v {op} TIME '12:00:00'");
        let got = ids(&run(
            &core,
            &format!("SELECT id FROM {TABLE} WHERE {pred} LIMIT 200"),
        ));
        assert_eq!(got, oracle(|x| f(x, 12 * H)), "{pred}");
    }
    // 型なし文字列リテラルとの式比較も同じ結果になる。
    let got = ids(&run(
        &core,
        &format!("SELECT id FROM {TABLE} WHERE v = TIME '24:00:00' LIMIT 200"),
    ));
    assert_eq!(got, oracle(|x| x == DAY));
    // 不正なリテラルは 22007、範囲外は 22008（値は応答に含めない）。
    assert_eq!(
        code(
            &core,
            &format!("SELECT id FROM {TABLE} WHERE v = 'zz' LIMIT 5")
        ),
        "22007"
    );
    assert_eq!(
        code(
            &core,
            &format!("SELECT id FROM {TABLE} WHERE v = '25:00:00' LIMIT 5")
        ),
        "22008"
    );
    assert_eq!(
        code(
            &core,
            &format!("SELECT id FROM {TABLE} WHERE v = TIME '25:00:00' LIMIT 5")
        ),
        "22008"
    );
}

#[test]
fn null_predicates() {
    let (core, _g) = open_core("tq-null");
    let nulls: Vec<u64> = (1..=ROWS).filter(|id| value_of(*id).is_none()).collect();
    assert_eq!(
        ids(&run(
            &core,
            &format!("SELECT id FROM {TABLE} WHERE v IS NULL LIMIT 200")
        )),
        nulls
    );
    assert_eq!(
        ids(&run(
            &core,
            &format!("SELECT id FROM {TABLE} WHERE v IS NOT NULL LIMIT 200")
        )),
        oracle(|_| true)
    );
}

#[test]
fn explain_uses_secondary_index_for_time() {
    let (core, _g) = open_core("tq-explain");
    let mut session = SessionState::default();
    let sql = format!(
        "EXPLAIN SELECT id FROM {TABLE} WHERE v > '23:59:59' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10"
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
fn tenant_private_rows_never_leak() {
    let (core, _g) = open_core("tq-rls");
    for w in [
        "v = '12:00:00'",
        "v >= '12:00:00'",
        "v BETWEEN '12:00:00' AND '12:00:00'",
        "v IN ('12:00:00')",
    ] {
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
    let agg = run(&core, &format!("SELECT COUNT(v) FROM {TABLE}"));
    assert_eq!(
        agg.rows[0].cells.first().map(cell_str),
        Some(oracle(|_| true).len().to_string())
    );
    let max = run(&core, &format!("SELECT MAX(v) FROM {TABLE}"));
    assert!(matches!(max.rows[0].cells.first(), Some(Cell::Time(t)) if *t == DAY));
}

fn cell_str(c: &Cell) -> String {
    match c {
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Integer(v) => v.to_string(),
        Cell::Float(v) => v.to_string(),
        Cell::Time(t) => engine::datetime::format_time(*t),
        Cell::Null => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

fn first_col(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| r.cells.first().map(cell_str).unwrap_or_default())
        .collect()
}

#[test]
fn order_by_follows_time_order_and_postgres_null_defaults() {
    let (core, _g) = open_core("tq-order");
    let mut present: Vec<i64> = (1..=ROWS).filter_map(value_of).collect();
    present.sort_unstable();
    let nulls = (1..=ROWS).filter(|id| value_of(*id).is_none()).count();
    let null_run = || std::iter::repeat_n("NULL".to_string(), nulls);

    let asc = first_col(&run(
        &core,
        &format!("SELECT v FROM {TABLE} ORDER BY v ASC, id ASC LIMIT 200"),
    ));
    let mut want: Vec<String> = present.iter().map(|v| lit(*v)).collect();
    want.extend(null_run());
    assert_eq!(asc, want, "ASC: NULLS LAST by default");

    let desc = first_col(&run(
        &core,
        &format!("SELECT v FROM {TABLE} ORDER BY v DESC, id ASC LIMIT 200"),
    ));
    let mut want: Vec<String> = null_run().collect();
    want.extend(present.iter().rev().map(|v| lit(*v)));
    assert_eq!(desc, want, "DESC: NULLS FIRST by default");
    assert_eq!(desc.get(nulls).map(String::as_str), Some("24:00:00"));
}

#[test]
fn group_by_distinct_count_distinct_and_having() {
    let (core, _g) = open_core("tq-group");
    let groups = run(
        &core,
        &format!("SELECT v, COUNT(*) FROM {TABLE} GROUP BY v ORDER BY v LIMIT 100"),
    );
    let mut expected: std::collections::BTreeMap<i64, i64> = Default::default();
    for id in 1..=ROWS {
        if let Some(v) = value_of(id) {
            *expected.entry(v).or_default() += 1;
        }
    }
    let got: Vec<(String, String)> = groups
        .rows
        .iter()
        .filter(|r| !matches!(r.cells.first(), Some(Cell::Null)))
        .map(|r| {
            (
                r.cells.first().map(cell_str).unwrap_or_default(),
                r.cells.get(1).map(cell_str).unwrap_or_default(),
            )
        })
        .collect();
    let want: Vec<(String, String)> = expected
        .iter()
        .map(|(k, c)| (lit(*k), c.to_string()))
        .collect();
    assert_eq!(got, want, "24:00:00 must remain its own group");

    let distinct = run(&core, &format!("SELECT DISTINCT v FROM {TABLE} LIMIT 100"));
    assert_eq!(distinct.rows.len(), expected.len() + 1, "values plus NULL");

    let cd = run(&core, &format!("SELECT COUNT(DISTINCT v) FROM {TABLE}"));
    assert_eq!(
        cd.rows[0].cells.first().map(cell_str),
        Some(expected.len().to_string())
    );

    let having = run(
        &core,
        &format!("SELECT v, COUNT(*) AS c FROM {TABLE} GROUP BY v HAVING c >= 7 LIMIT 100"),
    );
    let null_count = (1..=ROWS).filter(|id| value_of(*id).is_none()).count();
    assert_eq!(
        having.rows.len(),
        expected.values().filter(|c| **c >= 7).count() + usize::from(null_count >= 7)
    );
    // MIN/MAX(time) を HAVING の数値比較対象にすることは DATE/TIMESTAMP と同じく拒否する。
    assert!(core
        .execute_sql(
            &ctx("tenant-a"),
            &format!("SELECT v, MIN(v) AS lo FROM {TABLE} GROUP BY v HAVING lo < 1 LIMIT 100"),
        )
        .is_err());
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

#[test]
fn aggregates_min_max_count_and_sum_avg_are_rejected() {
    let (core, _g) = open_core("tq-agg");
    let r = run(
        &core,
        &format!("SELECT MIN(v), MAX(v), COUNT(v), COUNT(DISTINCT v) FROM {TABLE}"),
    );
    assert_eq!(
        computed_types(&r),
        vec![
            Some(ColumnType::Time),
            Some(ColumnType::Time),
            Some(ColumnType::BigInt),
            Some(ColumnType::BigInt),
        ]
    );
    let vals: Vec<i64> = (1..=ROWS).filter_map(value_of).collect();
    let mut distinct = vals.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let row: Vec<String> = r.rows[0].cells.iter().map(cell_str).collect();
    assert_eq!(
        row,
        vec![
            "00:00:00".to_string(),
            "24:00:00".to_string(),
            vals.len().to_string(),
            distinct.len().to_string(),
        ]
    );
    assert_eq!(code(&core, &format!("SELECT SUM(v) FROM {TABLE}")), "42883");
    assert_eq!(code(&core, &format!("SELECT AVG(v) FROM {TABLE}")), "42883");

    // window の PARTITION BY／ORDER BY キーと MIN(time) OVER。
    let w = run(
        &core,
        &format!(
            "SELECT COUNT(*) OVER (PARTITION BY v), MIN(v) OVER (ORDER BY v) FROM {TABLE} LIMIT 5"
        ),
    );
    assert_eq!(
        computed_types(&w),
        vec![Some(ColumnType::BigInt), Some(ColumnType::Time)]
    );
}

fn extract_one(core: &EngineCore, sql: &str) -> Option<f64> {
    match run(core, sql).rows[0].cells.first() {
        Some(Cell::Float(f)) => Some(*f),
        Some(Cell::Null) => None,
        other => panic!("unexpected cell {other:?} for {sql}"),
    }
}

#[test]
fn extract_and_date_part_on_time() {
    let (core, _g) = open_core("tq-extract");
    // id 5 → POOL[5] = 23:59:59.999999、id 6 → 24:00:00、id 4 → 12:34:56.789012、id 1 → 00:00:00.000001。
    let q = |f: &str, id: u64| {
        extract_one(
            &core,
            &format!("SELECT EXTRACT({f} FROM v) FROM {TABLE} WHERE id = {id} LIMIT 1"),
        )
    };
    assert_eq!(q("hour", 4), Some(12.0));
    assert_eq!(q("minute", 4), Some(34.0));
    assert!((q("second", 4).expect("second") - 56.789012).abs() < 1e-9);
    assert!((q("milliseconds", 4).expect("ms") - 56_789.012).abs() < 1e-6);
    assert!((q("microseconds", 4).expect("us") - 56_789_012.0).abs() < 1e-3);
    assert_eq!(q("hour", 5), Some(23.0));
    assert_eq!(q("hour", 6), Some(24.0), "24:00:00 must extract as hour 24");
    assert_eq!(q("minute", 6), Some(0.0));
    assert_eq!(q("epoch", 6), Some(86_400.0));
    assert_eq!(q("epoch", 1), Some(0.000001));
    // date_part 関数形・NULL の伝播・定数式。
    assert_eq!(
        extract_one(
            &core,
            &format!("SELECT date_part('hour', v) FROM {TABLE} WHERE id = 4 LIMIT 1")
        ),
        Some(12.0)
    );
    assert_eq!(q("hour", 9), None, "NULL propagates");
    assert_eq!(
        extract_one(
            &core,
            &format!("SELECT EXTRACT(hour FROM TIME '12:34:56') FROM {TABLE} LIMIT 1")
        ),
        Some(12.0)
    );
    // 日付由来の field は 0A000、date_trunc は存在しない関数（42883）、未知の field は 22000。
    for f in [
        "year", "month", "day", "dow", "doy", "week", "quarter", "isodow",
    ] {
        assert_eq!(
            code(
                &core,
                &format!("SELECT EXTRACT({f} FROM v) FROM {TABLE} LIMIT 1")
            ),
            "0A000",
            "{f}"
        );
    }
    assert_eq!(
        code(
            &core,
            &format!("SELECT date_trunc('hour', v) FROM {TABLE} LIMIT 1")
        ),
        "42883"
    );
    assert_eq!(
        code(
            &core,
            &format!("SELECT date_part('nosuch', v) FROM {TABLE} LIMIT 1")
        ),
        "22000"
    );
    // WHERE の式述語としても使える。
    let got = ids(&run(
        &core,
        &format!("SELECT id FROM {TABLE} WHERE EXTRACT(hour FROM v) = 24 LIMIT 200"),
    ));
    assert_eq!(got, oracle(|x| x == DAY));
}

#[test]
fn mixed_type_comparisons_and_arithmetic_stay_fail_closed() {
    let (core, _g) = open_core("tq-mismatch");
    for sql in [
        format!("SELECT CASE WHEN v = TIMESTAMP '2020-01-01 00:00:00' THEN 1 ELSE 0 END FROM {TABLE} LIMIT 1"),
        format!("SELECT CASE WHEN v = DATE '2020-01-01' THEN 1 ELSE 0 END FROM {TABLE} LIMIT 1"),
        format!("SELECT CASE WHEN v < m THEN 1 ELSE 0 END FROM {TABLE} LIMIT 1"),
    ] {
        assert_eq!(code(&core, &sql), "42883", "{sql}");
    }
    for sql in [
        format!("SELECT v + 1 FROM {TABLE} LIMIT 1"),
        format!("SELECT id FROM {TABLE} WHERE v = TIMESTAMP '2020-01-01 00:00:00' LIMIT 5"),
    ] {
        assert!(
            core.execute_sql(&ctx("tenant-a"), &sql).is_err(),
            "{sql} must be rejected"
        );
    }
}

#[test]
fn subquery_and_set_operations() {
    let (core, _g) = open_core("tq-sub-setop");
    let sub = run(
        &core,
        &format!("SELECT id FROM {TABLE} WHERE v = (SELECT MAX(v) FROM {TABLE}) LIMIT 200"),
    );
    assert_eq!(ids(&sub), oracle(|x| x == DAY));
    let sub_in = run(
        &core,
        &format!(
            "SELECT id FROM {TABLE} WHERE v IN (SELECT v FROM {TABLE} WHERE id = 6 LIMIT 10) LIMIT 200"
        ),
    );
    // id 6 → POOL[6] = 24:00:00。
    assert_eq!(ids(&sub_in), oracle(|x| x == DAY));

    let mut distinct: Vec<i64> = (1..=ROWS).filter_map(value_of).collect();
    distinct.sort_unstable();
    distinct.dedup();
    let union = run(
        &core,
        &format!("SELECT v FROM {TABLE} UNION SELECT v FROM {TABLE}"),
    );
    // NULL を含む 1 グループ分が加わる。
    assert_eq!(union.rows.len(), distinct.len() + 1);
    let union_all = run(
        &core,
        &format!(
            "SELECT v FROM {TABLE} WHERE id <= 8 UNION ALL SELECT v FROM {TABLE} WHERE id <= 8"
        ),
    );
    assert_eq!(union_all.rows.len(), 16);
    let intersect = run(
        &core,
        &format!(
            "SELECT v FROM {TABLE} WHERE id <= 8 INTERSECT SELECT v FROM {TABLE} WHERE id > 4 AND id <= 12"
        ),
    );
    assert!(!intersect.rows.is_empty());
    let except = run(
        &core,
        &format!("SELECT v FROM {TABLE} EXCEPT SELECT v FROM {TABLE} WHERE v = '24:00:00'"),
    );
    assert!(except
        .rows
        .iter()
        .all(|r| !matches!(r.cells.first(), Some(Cell::Time(t)) if *t == DAY)));
    // TIME と他の型の集合演算は型不一致。
    assert!(core
        .execute_sql(
            &ctx("tenant-a"),
            &format!("SELECT v FROM {TABLE} UNION SELECT m FROM {TABLE}")
        )
        .is_err());
}

#[test]
fn set_expr_assigns_time_values_without_side_effects() {
    let (core, _g) = open_core("tq-set");
    let mut session = SessionState::default();
    let mut exec = |sql: &str| core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql);
    exec("UPDATE docs SET v = CASE WHEN id = 2 THEN TIME '01:00:00' ELSE v END WHERE id = 2 USING OPERATION_ID 'u-ok'")
        .expect("assign TIME");
    let got = run(
        &core,
        &format!("SELECT v FROM {TABLE} WHERE id = 2 LIMIT 1"),
    );
    assert_eq!(first_col(&got), vec!["01:00:00".to_string()]);
    // 別型の式は 42804、副作用は残らない。
    let e = exec("UPDATE docs SET v = id + 1 WHERE id = 3 USING OPERATION_ID 'u-bad'")
        .expect_err("type mismatch");
    assert_eq!(e.wire_code(), "42804");
    let got = run(
        &core,
        &format!("SELECT v FROM {TABLE} WHERE id = 3 LIMIT 1"),
    );
    assert_eq!(first_col(&got), vec![lit(POOL[3])]);
}

/// CHECK 制約は式を `TIME '<format_time>'` で描画して保存し、再オープン時に再パースする。
/// `24:00:00` を含む型付きリテラルが往復で同じ意味になることを、再オープン後の拒否／受理で固定する。
#[test]
fn check_constraint_roundtrips_time_literals_across_reopen() {
    let (core, _g, path) = open_core_with_path("tq-roundtrip");
    let mut session = SessionState::default();
    session.allow_ddl();
    let exec = |core: &EngineCore, session: &mut SessionState, sql: &str| {
        core.execute_sql_in_session(&ctx("tenant-a"), session, sql)
    };
    exec(
        &core,
        &mut session,
        "CREATE TABLE c (embedding VECTOR(2), v TIME, CHECK (v >= TIME '01:00:00.5'), CHECK (v <= TIME '24:00:00'))",
    )
    .expect("CHECK with TIME literal");
    let insert = |core: &EngineCore, session: &mut SessionState, id: u64, v: &str, op: &str| {
        exec(
            core,
            session,
            &format!(
                "INSERT INTO c (id, embedding, v) VALUES ({id}, '[1.0,0.0]', {v}) USING OPERATION_ID '{op}'"
            ),
        )
    };
    insert(&core, &mut session, 1, "'24:00:00'", "c1").expect("24:00:00 satisfies both CHECKs");
    insert(&core, &mut session, 2, "'01:00:00.5'", "c2")
        .expect("boundary satisfies the lower CHECK");
    let e = insert(&core, &mut session, 3, "'01:00:00.4'", "c3").expect_err("below lower bound");
    assert_eq!(e.wire_code(), "23514");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    let e = insert(&core, &mut session, 4, "'00:00:00'", "c4").expect_err("CHECK survives reopen");
    assert_eq!(e.wire_code(), "23514");
    insert(&core, &mut session, 5, "'24:00:00'", "c5").expect("24:00:00 still accepted");
}

#[test]
fn prepared_where_on_time_describes_and_executes() {
    let (core, _g) = open_core("tq-prepared");
    let prepared = core
        .parse_sql_prepared(&format!("SELECT id FROM {TABLE} WHERE v = $1 LIMIT 200"))
        .expect("parse_sql_prepared");
    let session = SessionState::default();
    core.describe_prepared_in_session(&session, &prepared)
        .expect("describe with a TIME parameter");
    let bound = core
        .bind_prepared(&prepared, &[Some(b"24:00:00".to_vec())])
        .expect("bind");
    let mut session = SessionState::default();
    let out = core
        .execute_parsed_in_session(&ctx("tenant-a"), &mut session, &bound)
        .expect("execute");
    let SqlOutcome::Query(result) = out else {
        panic!("expected rows");
    };
    assert_eq!(ids(&result), oracle(|x| x == DAY));
}

/// INSERT VALUES の定数式で、裸の `NULL` と TIME の比較は NULL 伝播して ELSE 側を返す
/// （42883 にならない）。DATE／TIMESTAMP と同じ扱い（PR #2392 レビュー指摘の回帰）。
#[test]
fn bare_null_compared_with_time_propagates_in_const_expr() {
    let (core, _guard) = open_core("time-bare-null");
    let exprs = [
        "CASE WHEN TIME '12:00:00' = NULL THEN TIME '01:00:00' ELSE TIME '02:00:00' END",
        "CASE WHEN NULL = TIME '12:00:00' THEN TIME '01:00:00' ELSE TIME '02:00:00' END",
        "CASE WHEN TIME '12:00:00' < NULL THEN TIME '01:00:00' ELSE TIME '02:00:00' END",
        "COALESCE(NULL, TIME '02:00:00')",
    ];
    let mut session = SessionState::default();
    for (i, e) in exprs.iter().enumerate() {
        let id = 9000 + i;
        core.execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            &format!(
                "INSERT INTO {TABLE} (id, embedding, v) VALUES ({id}, '[1.0,0.0]', {e}) USING OPERATION_ID 'bn{i}'"
            ),
        )
        .unwrap_or_else(|err| panic!("insert must succeed: {e}: {err:?}"));
        let r = run(
            &core,
            &format!("SELECT v FROM {TABLE} WHERE id = {id} LIMIT 1"),
        );
        assert_eq!(first_col(&r), vec!["02:00:00".to_string()], "{e}");
    }
}

/// TIME の json 化（`to_json`）は未対応のため fail-closed で 0A000 になる。
#[test]
fn to_json_of_time_is_rejected_fail_closed() {
    let (core, _g) = open_core("tq-to-json");
    assert_eq!(
        code(&core, &format!("SELECT to_json(v) FROM {TABLE} LIMIT 1")),
        "0A000"
    );
}
