//! `GROUP BY` の `ROLLUP`・`CUBE`・`GROUPING SETS` と他機能の併用・上限境界の結合テスト
//! （Issue #1824、対象ビヘイビア: SQL-59。ポインタ: `docs/spec/04-behavior/` の SQL-59）。
//!
//! `EngineCore::execute_sql`（wire-server と NoSQL 表層が共有する production 経路）で、
//! (1) 集計の `FILTER` は入力行で評価されること、(2) 集計引数の `DISTINCT`・非分解集計
//! （`array_agg`）がセットごとに累積されること、(3) `HAVING`・`ORDER BY ... NULLS`・`LIMIT`・
//! `OFFSET`・`FETCH FIRST ... WITH TIES` が全セットの合成後に適用されること、(4) 結果行数上限・
//! グループ数上限が全セット合計に効き超過時は `Err` のみで部分結果を返さないこと、(5) RLS、
//! (6) 窓関数・`SELECT DISTINCT`・JOIN 等の未対応併用が fail-closed のままであること、を固定する。
//! 期待値は PostgreSQL 17 の意味論で手計算した固定値。基本の展開・合成は
//! `sql59_grouping_sets_parse.rs`／`sql59_grouping_sets_exec.rs`、`GROUPING()` 単体は
//! `sql59_grouping_function.rs` が担い、本ファイルはそれらと重複しない併用形に限る。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::group_by_limits::GroupByLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
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
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("kind", ColumnType::Text, true),
            ColumnDef::new("n", ColumnType::Integer, true),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// tenant-a は 6 行（`lang` が NULL の行・同じ (lang, kind) の重複行を含む）、tenant-b は
/// Private で（他テナントから不可視）tenant-a に無い `lang` 値（`fr`）を持つ 2 行。
fn build(
    name: &str,
    limits: Option<GroupByLimits>,
    result_limits: Option<ResultLimits>,
) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let rows: [(&str, u64, Option<&str>, &str, i32); 8] = [
        ("tenant-a", 1, Some("ja"), "a", 1),
        ("tenant-a", 2, Some("ja"), "a", 2),
        ("tenant-a", 3, Some("ja"), "b", 3),
        ("tenant-a", 4, Some("en"), "a", 4),
        ("tenant-a", 5, None, "a", 5),
        ("tenant-a", 6, None, "b", 6),
        ("tenant-b", 7, Some("fr"), "a", 100),
        ("tenant-b", 8, Some("ja"), "a", 1000),
    ];
    for (tenant, id, lang, kind, n) in rows {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_for(tenant),
            id,
            if tenant == "tenant-a" {
                Visibility::Public
            } else {
                Visibility::Private
            },
            &[
                Value::Vector(vec![id as f32, 0.0]),
                match lang {
                    Some(l) => Value::Text(l.to_string()),
                    None => Value::Null,
                },
                Value::Text(kind.to_string()),
                Value::Integer(n),
            ],
            &OperationId::parse(&format!("test-op-{id}")).expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match limits {
        Some(l) => core.with_group_by_limits(l),
        None => core,
    };
    let core = match result_limits {
        Some(l) => core.with_result_limits(l),
        None => core,
    };
    (core, guard)
}

fn cell_text(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Integer(i) => i.to_string(),
        Cell::SignedInteger(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

/// 結果行を `|` 区切りの文字列へ（行の順序を保つ）。
fn rows_as_text(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let result = core
        .execute_sql(&ctx_for(tenant), sql)
        .unwrap_or_else(|e| panic!("query must succeed: {sql}: {e:?}"));
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(cell_text).collect::<Vec<_>>().join("|"))
        .collect()
}

/// 結果行の集合を比較するための昇順ソート版（結果行の順序は契約にしない）。
fn sorted_rows(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let mut rows = rows_as_text(core, tenant, sql);
    rows.sort();
    rows
}

fn expect_sorted(mut expected: Vec<&str>) -> Vec<String> {
    expected.sort();
    expected.into_iter().map(str::to_string).collect()
}

fn err_code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    core.execute_sql(&ctx_for(tenant), sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

fn rows_a(core: &EngineCore, sql: &str) -> Vec<String> {
    rows_as_text(core, "tenant-a", sql)
}

fn sorted_a(core: &EngineCore, sql: &str) -> Vec<String> {
    sorted_rows(core, "tenant-a", sql)
}

fn code_a(core: &EngineCore, sql: &str) -> &'static str {
    err_code(core, "tenant-a", sql)
}

/// `FILTER` 述語は入力行の値で評価される（セット射影後の NULL では評価されない）。
/// 総計行でも `lang = 'ja'` の 3 行を数える。
#[test]
fn filter_evaluates_against_input_rows_not_projected_nulls() {
    let (core, _g) = build("sql59-combo-filter", None, None);
    assert_eq!(
        sorted_a(
            &core,
            "SELECT lang, count(*) FILTER (WHERE lang = 'ja'), sum(n) FILTER (WHERE kind = 'a') \
             FROM docs GROUP BY ROLLUP(lang)",
        ),
        expect_sorted(vec!["en|0|4", "ja|3|3", "NULL|0|5", "NULL|3|12"])
    );
    assert_eq!(
        sorted_a(
            &core,
            "SELECT lang, kind, count(*) FILTER (WHERE n > 2) FROM docs GROUP BY CUBE(lang, kind)",
        ),
        expect_sorted(vec![
            "en|a|1",
            "ja|a|0",
            "ja|b|1",
            "NULL|a|1",
            "NULL|b|1",
            "en|NULL|1",
            "ja|NULL|1",
            "NULL|NULL|2",
            "NULL|a|2",
            "NULL|b|2",
            "NULL|NULL|4",
        ])
    );
}

/// `FILTER` 付き集計と `GROUPING()` の併用（`GROUPING()` は `HAVING`、`FILTER` は選択リスト）。
#[test]
fn filter_combined_with_grouping_function() {
    let (core, _g) = build("sql59-combo-filter-grouping", None, None);
    assert_eq!(
        sorted_a(
            &core,
            "SELECT lang, grouping(lang), count(*) FILTER (WHERE n >= 3) FROM docs \
             GROUP BY ROLLUP(lang)",
        ),
        expect_sorted(vec!["en|0|1", "ja|0|1", "NULL|0|2", "NULL|1|4"])
    );
}

/// 集計引数の `DISTINCT` はセットごとに計算される（総計は小計の和ではない）。
#[test]
fn aggregate_distinct_is_computed_per_set() {
    let (core, _g) = build("sql59-combo-distinct", None, None);
    assert_eq!(
        sorted_a(
            &core,
            "SELECT lang, count(DISTINCT kind), count(DISTINCT n) FROM docs GROUP BY ROLLUP(lang)",
        ),
        expect_sorted(vec!["en|1|1", "ja|2|3", "NULL|2|2", "NULL|2|6"])
    );
    assert_eq!(
        sorted_a(
            &core,
            "SELECT lang, kind, count(DISTINCT n) FROM docs GROUP BY CUBE(lang, kind)",
        ),
        expect_sorted(vec![
            "en|a|1",
            "ja|a|2",
            "ja|b|1",
            "NULL|a|1",
            "NULL|b|1",
            "en|NULL|1",
            "ja|NULL|3",
            "NULL|NULL|2",
            "NULL|a|4",
            "NULL|b|2",
            "NULL|NULL|6",
        ])
    );
}

/// 非分解集計（`array_agg`）はセットごとに累積される（総計は全入力行を昇順で含む）。
#[test]
fn non_decomposable_aggregates_accumulate_per_set() {
    let (core, _g) = build("sql59-combo-array-agg", None, None);
    assert_eq!(
        sorted_a(
            &core,
            "SELECT lang, array_agg(n ORDER BY n) FROM docs GROUP BY ROLLUP(lang)",
        ),
        expect_sorted(vec![
            "NULL|Array(Integer([Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)]))",
            "NULL|Array(Integer([Some(5), Some(6)]))",
            "en|Array(Integer([Some(4)]))",
            "ja|Array(Integer([Some(1), Some(2), Some(3)]))",
        ])
    );
    // `DISTINCT` + `ORDER BY` の併用でも総計・小計・データ NULL グループが別々に累積される。
    assert_eq!(
        sorted_a(
            &core,
            "SELECT lang, array_agg(DISTINCT kind ORDER BY kind) FROM docs GROUP BY ROLLUP(lang)",
        ),
        expect_sorted(vec![
            "NULL|Array(Text([Some(\"a\"), Some(\"b\")]))",
            "NULL|Array(Text([Some(\"a\"), Some(\"b\")]))",
            "en|Array(Text([Some(\"a\")]))",
            "ja|Array(Text([Some(\"a\"), Some(\"b\")]))",
        ])
    );
}

/// データ上の NULL グループと小計・総計の NULL が `HAVING` の `grouping()` で区別できる
/// （`HAVING` は `IS NULL` を受理しないため、`grouping()` が区別手段になる）。
#[test]
fn having_distinguishes_data_null_from_subtotal_rows() {
    let (core, _g) = build("sql59-combo-having-null", None, None);
    assert_eq!(
        sorted_a(
            &core,
            "SELECT lang, count(*), grouping(lang) FROM docs GROUP BY ROLLUP(lang) \
             HAVING grouping(lang) = 0 AND count(*) > 1",
        ),
        expect_sorted(vec!["NULL|2|0", "ja|3|0"])
    );
    assert_eq!(
        sorted_a(
            &core,
            "SELECT lang, count(*), grouping(lang) FROM docs GROUP BY ROLLUP(lang) \
             HAVING grouping(lang) = 1",
        ),
        expect_sorted(vec!["NULL|6|1"])
    );
}

/// `ORDER BY ... NULLS FIRST/LAST`・`LIMIT`・`OFFSET` は全セット合成後に適用される。
/// データ NULL と小計 NULL は `lang` で同値のため、全順序が要る検査は `grouping(lang)` で補う。
#[test]
fn order_by_nulls_and_limit_apply_after_composition() {
    let (core, _g) = build("sql59-combo-order", None, None);
    let base = "SELECT lang, count(*), grouping(lang) FROM docs GROUP BY ROLLUP(lang)";
    assert_eq!(
        rows_a(
            &core,
            &format!("{base} ORDER BY grouping(lang), lang NULLS FIRST")
        ),
        vec!["NULL|2|0", "en|1|0", "ja|3|0", "NULL|6|1"]
    );
    assert_eq!(
        rows_a(
            &core,
            &format!("{base} ORDER BY grouping(lang), lang NULLS LAST")
        ),
        vec!["en|1|0", "ja|3|0", "NULL|2|0", "NULL|6|1"]
    );
    assert_eq!(
        rows_a(
            &core,
            &format!("{base} ORDER BY grouping(lang), lang DESC NULLS LAST"),
        ),
        vec!["ja|3|0", "en|1|0", "NULL|2|0", "NULL|6|1"]
    );
    assert_eq!(
        rows_a(
            &core,
            &format!("{base} ORDER BY grouping(lang), lang NULLS FIRST LIMIT 2 OFFSET 1"),
        ),
        vec!["en|1|0", "ja|3|0"]
    );
    // 序数指定。`NULLS FIRST` の先頭 2 行は NULL ブロック（データ NULL と総計）で、順序は問わない。
    let mut head = rows_a(
        &core,
        "SELECT lang, count(*) FROM docs GROUP BY ROLLUP(lang) ORDER BY 1 NULLS FIRST LIMIT 2",
    );
    head.sort();
    assert_eq!(head, vec!["NULL|2".to_string(), "NULL|6".to_string()]);
    assert_eq!(
        rows_a(
            &core,
            "SELECT lang, count(*) FROM docs GROUP BY ROLLUP(lang) ORDER BY count(*) DESC",
        ),
        vec!["NULL|6", "ja|3", "NULL|2", "en|1"]
    );
}

/// `FETCH FIRST ... WITH TIES` は同値の小計行も含めて返す。
#[test]
fn fetch_first_with_ties_includes_tied_subtotals() {
    let (core, _g) = build("sql59-combo-ties", None, None);
    assert_eq!(
        sorted_a(
            &core,
            "SELECT lang, kind, count(*) AS c FROM docs GROUP BY ROLLUP(lang, kind) \
             ORDER BY c FETCH FIRST 1 ROW WITH TIES",
        ),
        expect_sorted(vec![
            "en|a|1",
            "ja|b|1",
            "NULL|a|1",
            "NULL|b|1",
            "en|NULL|1"
        ])
    );
    assert_eq!(
        rows_a(
            &core,
            "SELECT lang, kind, count(*) AS c FROM docs GROUP BY ROLLUP(lang, kind) \
             ORDER BY c DESC FETCH FIRST 1 ROW ONLY",
        ),
        vec!["NULL|NULL|6"]
    );
}

/// 結果行数上限は全セット合計の行数（`HAVING`・`OFFSET` 適用後）で判定し、超過時は `Err` のみ
/// （部分結果を返さない）。`ROLLUP(lang, kind)` は合計 9 行。
#[test]
fn result_row_cap_counts_rows_across_sets_and_returns_no_partial_rows() {
    let base = "SELECT lang, kind, count(*) FROM docs GROUP BY ROLLUP(lang, kind)";
    let (exact, _g1) = build("sql59-combo-cap-exact", None, Some(ResultLimits::new(9)));
    assert_eq!(rows_a(&exact, base).len(), 9);
    assert_eq!(rows_a(&exact, &format!("{base} LIMIT ALL")).len(), 9);

    let (over, _g2) = build("sql59-combo-cap-over", None, Some(ResultLimits::new(8)));
    for sql in [base.to_string(), format!("{base} LIMIT ALL")] {
        let r = over.execute_sql(&ctx_for("tenant-a"), &sql);
        let e = r.expect_err("row cap must reject without returning partial rows");
        assert_eq!(e.wire_code(), "54000", "{sql}");
    }
    // `HAVING`・`OFFSET` 適用後に cap 以下なら成功する。
    assert_eq!(
        rows_a(&over, &format!("{base} HAVING count(*) > 1")).len(),
        4
    );
    assert_eq!(rows_a(&over, &format!("{base} OFFSET 1")).len(), 8);
    // 明示 `LIMIT n`（n <= cap）は成功する。
    assert_eq!(rows_a(&over, &format!("{base} LIMIT 8")).len(), 8);
}

/// `WITH TIES` で同値行を追加した結果が cap を超える場合は `54000`（部分結果なし）。
#[test]
fn with_ties_exceeding_the_cap_is_rejected() {
    let (core, _g) = build("sql59-combo-cap-ties", None, Some(ResultLimits::new(4)));
    let sql = "SELECT lang, kind, count(*) AS c FROM docs GROUP BY ROLLUP(lang, kind) \
               ORDER BY c FETCH FIRST 1 ROW WITH TIES";
    assert_eq!(code_a(&core, sql), "54000");
    let one = "SELECT lang, kind, count(*) AS c FROM docs GROUP BY ROLLUP(lang, kind) \
               ORDER BY c DESC FETCH FIRST 1 ROW ONLY";
    assert_eq!(rows_a(&core, one), vec!["NULL|NULL|6"]);
}

/// グループ数上限は `FILTER`・`DISTINCT` を併用しても全セット合計で判定される。
#[test]
fn group_limit_boundary_holds_with_filter_and_distinct() {
    let sql = "SELECT lang, kind, count(*) FILTER (WHERE n > 1), count(DISTINCT n) \
               FROM docs GROUP BY ROLLUP(lang, kind)";
    let (exact, _g1) = build(
        "sql59-combo-glimit-exact",
        Some(GroupByLimits::new(8, 9)),
        None,
    );
    assert_eq!(rows_a(&exact, sql).len(), 9);
    let (over, _g2) = build(
        "sql59-combo-glimit-over",
        Some(GroupByLimits::new(8, 8)),
        None,
    );
    assert_eq!(code_a(&over, sql), "54000");
}

/// RLS: `FILTER`・`DISTINCT`・`array_agg` を併用しても、tenant-a の結果のどのセットにも
/// tenant-b の Private 行の値が現れない。
#[test]
fn rls_holds_under_filter_and_distinct() {
    let (core, _g) = build("sql59-combo-rls", None, None);
    let sql = "SELECT lang, count(*) FILTER (WHERE n > 1), count(DISTINCT n), \
               array_agg(n ORDER BY n) FROM docs GROUP BY ROLLUP(lang)";
    let a = rows_as_text(&core, "tenant-a", sql).join("\n");
    for leaked in ["fr", "100", "1000"] {
        assert!(!a.contains(leaked), "tenant-b value {leaked} leaked: {a}");
    }
    assert_eq!(
        sorted_rows(
            &core,
            "tenant-b",
            "SELECT lang, count(*) FILTER (WHERE n > 100), count(DISTINCT n) \
             FROM docs GROUP BY ROLLUP(lang)",
        ),
        // tenant-b からは tenant-a の Public 行と自身の Private 行の両方が見える。
        expect_sorted(vec!["en|0|1", "fr|0|1", "ja|1|4", "NULL|0|2", "NULL|1|8"])
    );
}

/// 未対応の併用（窓関数・`SELECT DISTINCT`・JOIN・集合演算の枝）は fail-closed のまま。
#[test]
fn unsupported_combinations_stay_fail_closed() {
    let (core, _g) = build("sql59-combo-unsupported", None, None);
    for sql in [
        "SELECT lang, count(*) OVER () FROM docs GROUP BY ROLLUP(lang)",
        "SELECT lang, count(*) OVER () FROM docs GROUP BY CUBE(lang)",
        "SELECT lang, count(*) OVER () FROM docs GROUP BY GROUPING SETS ((lang), ())",
        "SELECT DISTINCT lang FROM docs GROUP BY ROLLUP(lang)",
        "SELECT lang FROM docs GROUP BY ROLLUP(lang) UNION ALL SELECT lang FROM docs",
        "SELECT a.lang, count(*) FROM docs a JOIN docs b ON a.id = b.id GROUP BY ROLLUP(a.lang)",
    ] {
        assert_eq!(code_a(&core, sql), "42601", "{sql}");
    }
}

/// `COUNT(DISTINCT)` の中間状態予算（異なり値 10 万件）は全セット合計で計上される。
/// 空のグルーピングセット `()` を 4,000 個並べ（1 セット 1 グループ）、各セットが `rows` 個の
/// 異なり値を保持する形にして、合計がちょうど上限（4,000 × 25）なら成功、1 行増えて超えれば
/// `54000`（`Err` のみで部分結果なし）になることを固定する。
fn distinct_budget_outcome(rows: i64) -> Result<usize, &'static str> {
    let path = unique_db_path("sql59-combo-dbudget");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n INTEGER)",
    )
    .expect("ddl");
    let values: Vec<String> = (1..=rows).map(|i| format!("({i}, {i})")).collect();
    core.execute_insert_sql(
        &ctx,
        &format!("INSERT INTO t (id, n) VALUES {}", values.join(", ")),
    )
    .expect("seed");
    let list = vec!["()"; 4000].join(", ");
    let sql = format!("SELECT count(DISTINCT n) FROM t GROUP BY GROUPING SETS ({list})");
    core.execute_sql(&ctx, &sql)
        .map(|r| r.rows.len())
        .map_err(|e| e.wire_code())
}

#[test]
fn count_distinct_budget_is_charged_across_all_sets() {
    assert_eq!(distinct_budget_outcome(25), Ok(4000));
    assert_eq!(distinct_budget_outcome(26), Err("54000"));
}
