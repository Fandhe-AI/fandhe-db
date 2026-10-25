//! `bool_and` / `bool_or` / `every` の結合テスト（SQL-57・Issue #1805。関連: SQL-13・SQL-14・
//! RLS-7/RLS-8・WIRE-13）。
//!
//! 期待値は PostgreSQL 17 の意味論（NULL 入力は無視・全行 NULL と入力なしは NULL・結果は boolean・
//! `every` は `bool_and` と同値で既定列名だけが `every`）に従う固定値。`unique_db_path`＋
//! `CleanupGuard`、`EngineCore::execute_sql` の production 経路で検証し、他テナントの Private 行
//! （結果を反転させる偽値）が結果へ影響しないこと（RLS）と、未対応形の fail-closed な
//! `wire_code` を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("g", ColumnType::Text, true),
            ColumnDef::new("k", ColumnType::Integer, true),
            ColumnDef::new("f", ColumnType::Boolean, true),
            ColumnDef::new("dt", ColumnType::Date, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `(id, g, k, f, dt)`。`k`・`f`・`dt` の NULL を含む。
type Seed = (u64, &'static str, Option<i32>, Option<bool>, Option<i32>);

const SEEDS: [Seed; 8] = [
    (1, "a", Some(3), Some(true), Some(19_001)),
    (2, "a", Some(1), Some(true), Some(19_000)),
    (3, "a", None, None, None),
    (4, "b", Some(5), Some(false), Some(19_005)),
    (5, "b", Some(2), Some(true), Some(19_002)),
    (6, "c", None, None, None),
    (7, "d", Some(4), Some(false), Some(19_004)),
    (8, "d", None, Some(false), None),
];

fn insert(storage: &Storage, tenant: &str, seed: &Seed, vis: Visibility) {
    let (id, g, k, f, dt) = *seed;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(g.to_string()),
            k.map_or(Value::Null, Value::Integer),
            f.map_or(Value::Null, Value::Bool),
            dt.map_or(Value::Null, Value::Date),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn build_core(tag: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(tag);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for seed in &SEEDS {
        insert(&storage, "tenant-a", seed, Visibility::Public);
    }
    // 他テナントの Private 行。見えてしまうと bool_and は偽・bool_or は真へ変わる値を入れる。
    insert(
        &storage,
        "tenant-b",
        &(100, "c", Some(0), Some(false), Some(10_000)),
        Visibility::Private,
    );
    insert(
        &storage,
        "tenant-b",
        &(101, "a", Some(0), Some(false), Some(10_000)),
        Visibility::Private,
    );
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"))
}

fn code_of(core: &EngineCore, sql: &str) -> &'static str {
    core.execute_sql(&ctx("tenant-a"), sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

fn cell_bool(c: &Cell) -> Option<bool> {
    match c {
        Cell::Bool(b) => Some(*b),
        Cell::Null => None,
        other => panic!("expected bool or NULL, got {other:?}"),
    }
}

/// 単一行・単一列の boolean（NULL は `None`）。
fn one(core: &EngineCore, sql: &str) -> Option<bool> {
    let r = run(core, sql);
    assert_eq!(r.rows.len(), 1, "{sql}");
    cell_bool(&r.rows[0].cells[0])
}

/// `GROUP BY g` の結果を `(g, 値...)` の並びで返す。
fn groups(core: &EngineCore, sql: &str) -> Vec<(String, Vec<Option<bool>>)> {
    run(core, sql)
        .rows
        .iter()
        .map(|row| {
            let g = match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected text key, got {other:?}"),
            };
            (g, row.cells[1..].iter().map(cell_bool).collect())
        })
        .collect()
}

/// 単一表の全体集計: NULL は無視され、`every` は `bool_and` と同値。
#[test]
fn global_semantics_ignore_nulls_and_every_equals_bool_and() {
    let (core, path) = build_core("sql57-bool-global");
    let _g = CleanupGuard(path);
    assert_eq!(one(&core, "SELECT bool_and(f) FROM t"), Some(false));
    assert_eq!(one(&core, "SELECT bool_or(f) FROM t"), Some(true));
    assert_eq!(one(&core, "SELECT every(f) FROM t"), Some(false));
    // 全行が真・全行が偽の部分集合。
    assert_eq!(
        one(&core, "SELECT bool_and(f) FROM t WHERE g = 'a'"),
        Some(true)
    );
    assert_eq!(
        one(&core, "SELECT every(f) FROM t WHERE g = 'a'"),
        Some(true)
    );
    assert_eq!(
        one(&core, "SELECT bool_or(f) FROM t WHERE g = 'd'"),
        Some(false)
    );
    // NULL と真の混在: NULL は無視されるので bool_and は真。
    assert_eq!(
        one(&core, "SELECT bool_and(f) FROM t WHERE id IN (1, 3)"),
        Some(true)
    );
}

/// 全行 NULL・入力なし（WHERE で 0 行）はいずれも NULL。
#[test]
fn all_null_and_empty_input_are_null() {
    let (core, path) = build_core("sql57-bool-null");
    let _g = CleanupGuard(path);
    for func in ["bool_and", "bool_or", "every"] {
        assert_eq!(
            one(&core, &format!("SELECT {func}(f) FROM t WHERE g = 'c'")),
            None,
            "{func} all NULL"
        );
        assert_eq!(
            one(&core, &format!("SELECT {func}(f) FROM t WHERE g = 'none'")),
            None,
            "{func} empty"
        );
    }
}

/// 既定列名（`every` は `every`）・`AS` 別名・結果列の型は boolean。
#[test]
fn default_names_alias_and_boolean_result_type() {
    let (core, path) = build_core("sql57-bool-names");
    let _g = CleanupGuard(path);
    let r = run(
        &core,
        "SELECT bool_and(f), bool_or(f), every(f), bool_and(f) AS x FROM t",
    );
    let names: Vec<(&str, bool)> = r
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { name, ty } => {
                (name.as_str(), matches!(ty, Some(ColumnType::Boolean)))
            }
            other => panic!("unexpected column meta: {other:?}"),
        })
        .collect();
    assert_eq!(
        names,
        vec![
            ("bool_and", true),
            ("bool_or", true),
            ("every", true),
            ("x", true)
        ]
    );
}

/// 比較式の引数: NULL 比較は無視され、日付どうしの比較・text の比較も受理する。
#[test]
fn comparison_arguments() {
    let (core, path) = build_core("sql57-bool-cmp");
    let _g = CleanupGuard(path);
    // k = 3,1,5,2,4（NULL 3 行は比較が NULL になり無視）。
    assert_eq!(one(&core, "SELECT bool_and(k > 1) FROM t"), Some(false));
    assert_eq!(one(&core, "SELECT bool_or(k > 4) FROM t"), Some(true));
    assert_eq!(one(&core, "SELECT every(k >= 1) FROM t"), Some(true));
    assert_eq!(one(&core, "SELECT bool_or(k < 1) FROM t"), Some(false));
    assert_eq!(
        one(&core, "SELECT bool_and(k > 1) FROM t WHERE g = 'c'"),
        None
    );
    assert_eq!(one(&core, "SELECT bool_or(g = 'a') FROM t"), Some(true));
    assert_eq!(one(&core, "SELECT bool_and(g = 'a') FROM t"), Some(false));
    assert_eq!(
        one(&core, "SELECT bool_and(dt > dt) FROM t WHERE g = 'b'"),
        Some(false)
    );
    assert_eq!(one(&core, "SELECT bool_and(k + 1 > k) FROM t"), Some(true));
}

/// GROUP BY: グループごとの結果。全 NULL のグループは NULL。同じ SELECT の `COUNT(<boolean 列>)` は
/// 従来どおり非 NULL 件数を返す。
#[test]
fn group_by_results_and_count_regression() {
    let (core, path) = build_core("sql57-bool-group");
    let _g = CleanupGuard(path);
    let mut got = groups(
        &core,
        "SELECT g, bool_and(f), bool_or(f), every(f), bool_and(k > 1) FROM t GROUP BY g",
    );
    got.sort();
    assert_eq!(
        got,
        vec![
            (
                "a".to_string(),
                vec![Some(true), Some(true), Some(true), Some(false)]
            ),
            (
                "b".to_string(),
                vec![Some(false), Some(true), Some(false), Some(true)]
            ),
            ("c".to_string(), vec![None, None, None, None]),
            (
                "d".to_string(),
                vec![Some(false), Some(false), Some(false), Some(true)]
            ),
        ]
    );
    let r = run(&core, "SELECT COUNT(f), bool_or(f) FROM t");
    assert_eq!(r.rows[0].cells[0], Cell::Integer(6));
    assert_eq!(cell_bool(&r.rows[0].cells[1]), Some(true));
}

/// 結果の ORDER BY: false < true、NULL は ASC で末尾・DESC で先頭。
#[test]
fn order_by_boolean_result() {
    let (core, path) = build_core("sql57-bool-order");
    let _g = CleanupGuard(path);
    let keys =
        |sql: &str| -> Vec<String> { groups(&core, sql).into_iter().map(|(g, _)| g).collect() };
    assert_eq!(
        keys("SELECT g, bool_and(f) AS ba FROM t GROUP BY g ORDER BY ba, g"),
        vec!["b", "d", "a", "c"]
    );
    assert_eq!(
        keys("SELECT g, bool_and(f) AS ba FROM t GROUP BY g ORDER BY ba DESC, g"),
        vec!["c", "a", "b", "d"]
    );
}

/// HAVING: bool 系の項目と別項目の HAVING が共存しても内部エラーにならず、bool 系の結果を数値比較すると
/// `22000`（fail-closed）。
#[test]
fn having_coexistence_and_rejection() {
    let (core, path) = build_core("sql57-bool-having");
    let _g = CleanupGuard(path);
    let r = run(
        &core,
        "SELECT g, bool_and(f), COUNT(*) AS c FROM t GROUP BY g HAVING c > 1 ORDER BY g",
    );
    let got: Vec<(String, Option<bool>)> = r
        .rows
        .iter()
        .map(|row| match &row.cells[0] {
            Cell::Text(g) => (g.clone(), cell_bool(&row.cells[1])),
            other => panic!("expected text key, got {other:?}"),
        })
        .collect();
    assert_eq!(
        got,
        vec![
            ("a".to_string(), Some(true)),
            ("b".to_string(), Some(false)),
            ("d".to_string(), Some(false))
        ]
    );
    assert_eq!(
        code_of(
            &core,
            "SELECT g, bool_and(f) AS ba FROM t GROUP BY g HAVING ba > 0"
        ),
        "22000"
    );
}

/// 未対応形は fail-closed で拒否する。
#[test]
fn unsupported_forms_are_rejected() {
    let (core, path) = build_core("sql57-bool-reject");
    let _g = CleanupGuard(path);
    // 非 boolean の列・式・id は該当シグネチャの関数が無い。
    assert_eq!(code_of(&core, "SELECT bool_and(k) FROM t"), "42883");
    assert_eq!(code_of(&core, "SELECT bool_or(g) FROM t"), "42883");
    assert_eq!(code_of(&core, "SELECT every(id) FROM t"), "42883");
    assert_eq!(code_of(&core, "SELECT bool_and(k + 1) FROM t"), "42883");
    // 2 引数・未知列。
    assert_eq!(code_of(&core, "SELECT bool_and(f, f) FROM t"), "42883");
    assert_eq!(code_of(&core, "SELECT bool_and(nope) FROM t"), "42703");
    // `*`・DISTINCT・集計内 ORDER BY・FILTER・ウィンドウ・空引数は構文エラー。
    assert_eq!(code_of(&core, "SELECT bool_and(*) FROM t"), "42601");
    assert_eq!(
        code_of(&core, "SELECT bool_and(DISTINCT f) FROM t"),
        "42601"
    );
    assert_eq!(
        code_of(&core, "SELECT bool_and(f ORDER BY k) FROM t"),
        "42601"
    );
    assert_eq!(
        code_of(&core, "SELECT bool_and(f) FILTER (WHERE k > 1) FROM t"),
        "42601"
    );
    assert_eq!(code_of(&core, "SELECT bool_and(f) OVER () FROM t"), "42601");
    assert_eq!(code_of(&core, "SELECT bool_and() FROM t"), "42601");
    // 論理演算を含む引数は本 Issue の対象外。
    assert_eq!(code_of(&core, "SELECT bool_and(f AND f) FROM t"), "42601");
    // FROM なしは対象外。
    assert_eq!(code_of(&core, "SELECT bool_and(1)"), "0A000");
}

/// RLS: 他テナントの Private 行（偽の値）が結果へ影響しない。
#[test]
fn other_tenant_rows_do_not_affect_results() {
    let (core, path) = build_core("sql57-bool-rls");
    let _g = CleanupGuard(path);
    assert_eq!(
        one(&core, "SELECT bool_and(f) FROM t WHERE g = 'a'"),
        Some(true)
    );
    // グループ `c` は自テナントでは全行 NULL（他テナントの偽値が見えれば false になる）。
    assert_eq!(one(&core, "SELECT bool_or(f) FROM t WHERE g = 'c'"), None);
    assert_eq!(
        one(&core, "SELECT bool_and(k > 0) FROM t WHERE g = 'a'"),
        Some(true)
    );
    // 別テナントから見れば別の結果になる（自分の行だけが対象）。
    let r = core
        .execute_sql(&ctx("tenant-b"), "SELECT bool_and(f) FROM t WHERE g = 'a'")
        .expect("tenant-b query");
    assert_eq!(cell_bool(&r.rows[0].cells[0]), Some(false));
}

/// 予約名: 組み込み集計と同名の UDF は作成できない。`every` という列名は括弧なしなら使える。
#[test]
fn function_names_are_reserved() {
    let (core, path) = build_core("sql57-bool-reserved");
    let _g = CleanupGuard(path);
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    for name in ["bool_and", "bool_or", "every"] {
        let err = core
            .execute_sql_in_session(
                &ctx("tenant-a"),
                &mut session,
                &format!(
                    "CREATE FUNCTION {name}(x DOUBLE PRECISION) RETURNS DOUBLE PRECISION AS x * 2"
                ),
            )
            .expect_err("reserved name must be rejected");
        assert!(
            matches!(err.wire_code(), "42723" | "42601" | "0A000"),
            "{name}: {}",
            err.wire_code()
        );
    }
}
