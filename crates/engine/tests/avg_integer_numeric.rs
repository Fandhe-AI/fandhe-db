//! 整数型（SMALLINT／INTEGER／BIGINT）と疑似列 `id` の `AVG` が numeric を返す契約
//! （TABLE-13・TASK-330、Issue #2270。`docs/spec/04-behavior/` のポインタのみ）の結合テスト。
//! 値と小数部の桁数が PostgreSQL 17 と同じこと、`GROUP BY` の `ORDER BY`／`HAVING` が
//! 値ごとに scale の異なる結果を厳密に扱うこと、スカラーサブクエリ比較・窓関数・
//! テナント境界を固定する。値はすべて `Decimal` で厳密に比較する（近似比較しない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    match core
        .execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"))
    {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query for {sql:?}, got {other:?}"),
    }
}

/// 結果のセルを PG のテキスト表現（`Decimal::Display`）へ写す。NULL は `NULL`。
fn texts(r: &QueryResult) -> Vec<Vec<String>> {
    r.rows
        .iter()
        .map(|row| {
            row.cells
                .iter()
                .map(|c| match c {
                    Cell::Null => "NULL".to_string(),
                    Cell::Numeric(d) => d.to_string(),
                    Cell::Text(s) => s.clone(),
                    Cell::Integer(n) => n.to_string(),
                    Cell::SignedInteger(n) => n.to_string(),
                    other => panic!("unexpected cell {other:?}"),
                })
                .collect()
        })
        .collect()
}

fn col(r: &QueryResult, rows: usize, i: usize) -> Vec<String> {
    texts(r)
        .into_iter()
        .take(rows)
        .map(|row| row[i].clone())
        .collect()
}

/// 表 `t(g TEXT, n INTEGER, b BIGINT, s SMALLINT, d DOUBLE)`。テナント `t1` の行:
/// g=a は n=1,2（平均 1.5）、g=b は n=1,1（平均 1）、g=c は n=1,1,2（平均 4/3）、
/// g=d は n=-3,-4（平均 -3.5）。別テナント `t2` の行（n=1000）は結果に現れてはならない。
fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path("avg-integer-numeric");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("g", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("b", ColumnType::BigInt, false),
                ColumnDef::new("s", ColumnType::SmallInt, false),
                ColumnDef::new("d", ColumnType::Double, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    let rows: [(u64, &str, i64); 9] = [
        (1, "a", 1),
        (2, "a", 2),
        (3, "b", 1),
        (4, "b", 1),
        (5, "c", 1),
        (6, "c", 1),
        (7, "c", 2),
        (8, "d", -3),
        (9, "d", -4),
    ];
    for (id, g, n) in rows {
        core.execute_sql_in_session(
            &ctx,
            &mut SessionState::default(),
            &format!(
                "INSERT INTO t (id, embedding, g, n, b, s, d) VALUES \
                 ({id}, '[0.{id},0.1]', '{g}', {n}, {n}, {n}, {n}.5) USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    }
    core.execute_sql_in_session(
        &ctx_for("t2"),
        &mut SessionState::default(),
        "INSERT INTO t (id, embedding, g, n, b, s, d) VALUES \
         (99, '[0.9,0.1]', 'a', 1000, 1000, 1000, 1000.5) USING OPERATION_ID 'seed-other'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    (core, ctx, CleanupGuard(path))
}

/// SQL-13・TABLE-13: 3 整数型の `AVG` は PG 17 と同じ値・小数部の桁数で numeric を返す。
#[test]
fn avg_of_each_integer_type_matches_postgres_value_and_scale() {
    let (core, ctx, _g) = setup();
    // t1 の n: 合計 2、行数 9 → 2/9 = 0.2222...（先頭桁規則で小数 20 桁）。
    for column in ["n", "b", "s"] {
        let r = run(&core, &ctx, &format!("SELECT AVG({column}) FROM t"));
        assert_eq!(
            texts(&r),
            vec![vec!["0.22222222222222222222".to_string()]],
            "{column}"
        );
    }
    // 1.5 / 1（割り切れる）/ 4/3（丸めなし 16 桁）/ 負数。
    let r = run(&core, &ctx, "SELECT AVG(n) FROM t WHERE g = 'a'");
    assert_eq!(texts(&r), vec![vec!["1.5000000000000000".to_string()]]);
    let r = run(&core, &ctx, "SELECT AVG(n) FROM t WHERE g = 'b'");
    assert_eq!(texts(&r), vec![vec!["1.00000000000000000000".to_string()]]);
    let r = run(&core, &ctx, "SELECT AVG(n) FROM t WHERE g = 'c'");
    assert_eq!(texts(&r), vec![vec!["1.3333333333333333".to_string()]]);
    let r = run(&core, &ctx, "SELECT AVG(n) FROM t WHERE g = 'd'");
    assert_eq!(texts(&r), vec![vec!["-3.5000000000000000".to_string()]]);
    // 1 行・0 の平均・空集合（NULL）・疑似列 id（1+2 = 3、行数 2）。
    let r = run(&core, &ctx, "SELECT AVG(n) FROM t WHERE id = 1");
    assert_eq!(texts(&r), vec![vec!["1.00000000000000000000".to_string()]]);
    let r = run(&core, &ctx, "SELECT AVG(n) FROM t WHERE g = 'zz'");
    assert_eq!(texts(&r), vec![vec!["NULL".to_string()]]);
    let r = run(&core, &ctx, "SELECT AVG(id) FROM t WHERE g = 'a'");
    assert_eq!(texts(&r), vec![vec!["1.5000000000000000".to_string()]]);
    // DOUBLE の AVG は従来どおり float8。
    let r = run(&core, &ctx, "SELECT AVG(d) FROM t WHERE g = 'a'");
    assert!(matches!(r.rows[0].cells[0], Cell::Float(_)));
}

/// 結果型は numeric（Describe 相当の列メタ）で、セルは `Decimal`。
#[test]
fn avg_of_integer_is_announced_as_numeric() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT AVG(n) AS a FROM t");
    match &r.columns[0] {
        engine::sql::exec::ColumnMeta::Computed { ty, .. } => {
            assert!(matches!(ty, Some(ColumnType::Numeric { .. })), "{ty:?}");
        }
        other => panic!("expected Computed, got {other:?}"),
    }
    assert!(matches!(r.rows[0].cells[0], Cell::Numeric(_)));
}

/// 他テナント（n=1000）の行は平均に混ざらない（RLS）。混入すれば値が大きく変わる。
#[test]
fn avg_ignores_other_tenants_rows() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT AVG(n) FROM t WHERE g = 'a'");
    assert_eq!(texts(&r), vec![vec!["1.5000000000000000".to_string()]]);
    let other = run(&core, &ctx_for("t2"), "SELECT AVG(n) FROM t");
    assert_eq!(
        texts(&other),
        vec![vec!["1000.0000000000000000".to_string()]]
    );
}

/// グループ間で scale が異なる結果（1.5@16 と 1@20）でも `ORDER BY` が厳密に効く。
#[test]
fn group_by_order_by_sorts_values_with_different_scales() {
    let (core, ctx, _g) = setup();
    let asc = run(
        &core,
        &ctx,
        "SELECT g, AVG(n) AS a FROM t GROUP BY g ORDER BY a",
    );
    assert_eq!(col(&asc, 4, 0), vec!["d", "b", "c", "a"]);
    let desc = run(
        &core,
        &ctx,
        "SELECT g, AVG(n) AS a FROM t GROUP BY g ORDER BY a DESC",
    );
    assert_eq!(col(&desc, 4, 0), vec!["a", "c", "b", "d"]);
}

/// `HAVING <別名> <比較演算子> <リテラル>` は scale を問わず厳密に比較する（黙って偽に縮退しない）。
#[test]
fn having_compares_integer_avg_exactly() {
    let (core, ctx, _g) = setup();
    let groups = |having: &str| -> Vec<String> {
        let r = run(
            &core,
            &ctx,
            &format!("SELECT g, AVG(n) AS a FROM t GROUP BY g HAVING a {having} ORDER BY g"),
        );
        col(&r, r.rows.len(), 0)
    };
    assert_eq!(groups("= 1.5"), vec!["a"]);
    assert_eq!(groups("> 1.5"), Vec::<String>::new());
    assert_eq!(groups(">= 1.5"), vec!["a"]);
    assert_eq!(groups("> 1"), vec!["a", "c"]);
    assert_eq!(groups(">= 1"), vec!["a", "b", "c"]);
    assert_eq!(groups("<= 1"), vec!["b", "d"]);
    assert_eq!(groups("< 1.34"), vec!["b", "c", "d"]);
    assert_eq!(groups("= 1.3333333333333333"), vec!["c"]);
}

/// 窓関数 `AVG(...) OVER` も同じ型・同じ値を返す。
#[test]
fn window_avg_of_integer_returns_numeric() {
    let (core, ctx, _g) = setup();
    let r = run(
        &core,
        &ctx,
        "SELECT AVG(n) OVER (PARTITION BY g) AS a FROM t WHERE g = 'c' LIMIT 10",
    );
    assert_eq!(r.rows.len(), 3);
    for row in &r.rows {
        let Cell::Numeric(d) = &row.cells[0] else {
            panic!("expected numeric, got {:?}", row.cells[0]);
        };
        assert_eq!(d.to_string(), "1.3333333333333333");
    }
}

/// スカラーサブクエリ比較: 端数のある numeric を整数列・DOUBLE 列と厳密に比べる。
#[test]
fn scalar_subquery_compares_integer_avg_with_integer_and_double_columns() {
    let (core, ctx, _g) = setup();
    // 全体の平均は 0.2222...。n > 0.22 ⇔ n >= 1。
    let ids = |sql: &str| -> Vec<String> {
        let r = run(&core, &ctx, sql);
        col(&r, r.rows.len(), 0)
    };
    assert_eq!(
        ids("SELECT id FROM t WHERE n > (SELECT AVG(n) FROM t) ORDER BY id LIMIT 100"),
        vec!["1", "2", "3", "4", "5", "6", "7"]
    );
    assert_eq!(
        ids("SELECT id FROM t WHERE n < (SELECT AVG(n) FROM t) ORDER BY id LIMIT 100"),
        vec!["8", "9"]
    );
    // 端数のある平均と整数列の `=` は常に偽、`<>` は全行。
    assert!(ids("SELECT id FROM t WHERE n = (SELECT AVG(n) FROM t) LIMIT 100").is_empty());
    assert_eq!(
        ids("SELECT id FROM t WHERE n <> (SELECT AVG(n) FROM t) LIMIT 100").len(),
        9
    );
    // g='a' の平均 1.5 と整数列: n >= 1.5 ⇔ n >= 2。
    assert_eq!(
        ids(
            "SELECT id FROM t WHERE n >= (SELECT AVG(n) FROM t WHERE g = 'a') \
             ORDER BY id LIMIT 100"
        ),
        vec!["2", "7"]
    );
    // DOUBLE 列との比較（d = n + 0.5）。d > 1.5 ⇔ n >= 2。
    assert_eq!(
        ids(
            "SELECT id FROM t WHERE d > (SELECT AVG(n) FROM t WHERE g = 'a') \
             ORDER BY id LIMIT 100"
        ),
        vec!["2", "7"]
    );
}
