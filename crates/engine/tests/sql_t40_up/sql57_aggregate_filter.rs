//! 集計の `FILTER (WHERE ...)` 句の結合テスト（SQL-57・Issue #1806。関連: SQL-13・SQL-14・
//! SQL-25 (c)・SQL-29・SQL-61・RLS-7/RLS-8）。
//!
//! 期待値は PostgreSQL 17 の意味論（条件が偽・NULL の行は集計へ入れない・全行除外で `count` は 0・
//! それ以外は NULL・条件中の集計は `42803`・ウィンドウは `42P20`）に従う固定値。`unique_db_path`＋
//! `CleanupGuard`、`EngineCore::execute_sql` の production 経路で検証し、他テナントの Private 行が
//! FILTER 付き集計（部分クエリの内側を含む）へ影響しないこと（RLS）と `wire_code` のエラー契約を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("g", ColumnType::Text, true),
            ColumnDef::new("s", ColumnType::Text, true),
            ColumnDef::new("k", ColumnType::Integer, true),
            ColumnDef::new("bo", ColumnType::Boolean, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `(id, g, s, k)`。`s`・`k` の NULL を含む。`bo` は `id` が偶数のとき真。
type Seed = (u64, &'static str, Option<&'static str>, Option<i32>);

const SEEDS: [Seed; 8] = [
    (1, "a", Some("x"), Some(3)),
    (2, "a", None, Some(1)),
    (3, "a", Some("y"), Some(2)),
    (4, "b", Some("z"), Some(5)),
    (5, "b", Some("w"), None),
    (6, "c", None, Some(1)),
    (7, "b", Some("x"), Some(4)),
    (8, "a", Some("x"), Some(9)),
];

fn insert_t(storage: &Storage, tenant: &str, seed: &Seed, vis: Visibility) {
    let (id, g, s, k) = *seed;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(g.to_string()),
            s.map_or(Value::Null, |v| Value::Text(v.to_string())),
            k.map_or(Value::Null, Value::Integer),
            Value::Bool(id % 2 == 0),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn build_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for seed in &SEEDS {
        insert_t(&storage, "tenant-a", seed, Visibility::Public);
    }
    // 他テナントの Private 行（tenant-a からは見えてはならない。FILTER 条件に一致する値を持つ）。
    insert_t(
        &storage,
        "tenant-b",
        &(100, "a", Some("SECRET"), Some(1000)),
        Visibility::Private,
    );
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn cell_str(c: &Cell) -> String {
    match c {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(n) => n.to_string(),
        Cell::SignedInteger(n) => n.to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Float(f) => format!("{f}"),
        Cell::Bool(b) => b.to_string(),
        other => format!("{other:?}"),
    }
}

fn rows_of(r: &QueryResult) -> Vec<Vec<String>> {
    r.rows
        .iter()
        .map(|row| row.cells.iter().map(cell_str).collect())
        .collect()
}

fn run_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<String>> {
    let r = core
        .execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"));
    rows_of(&r)
}

fn run(core: &EngineCore, sql: &str) -> Vec<Vec<String>> {
    run_as(core, "tenant-a", sql)
}

/// 単一行の結果を 1 行分の文字列列で返す。
fn one(core: &EngineCore, sql: &str) -> Vec<String> {
    let mut rows = run(core, sql);
    assert_eq!(rows.len(), 1, "{sql}");
    rows.remove(0)
}

fn code_of(core: &EngineCore, sql: &str) -> &'static str {
    core.execute_sql(&ctx("tenant-a"), sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl must succeed: {sql}: {e:?}"));
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// 全集計関数（count/sum/avg/min/max）に効き、FILTER なしの項目と同一文で混在できる。
#[test]
fn filter_applies_to_every_aggregate_function() {
    let (core, _g) = build_core("sql57-flt-funcs");
    assert_eq!(
        one(
            &core,
            "SELECT count(*) FILTER (WHERE g = 'a'), count(*) FILTER (WHERE g = 'b'), count(*) FROM t"
        ),
        strs(&["4", "3", "8"])
    );
    assert_eq!(
        one(
            &core,
            "SELECT sum(k) FILTER (WHERE g = 'a'), min(k) FILTER (WHERE g = 'a'), \
             max(k) FILTER (WHERE g = 'a'), count(k) FILTER (WHERE g = 'b') FROM t"
        ),
        strs(&["15", "1", "9", "2"])
    );
    // `avg` は FILTER 後の 4 行（3,1,2,9）の平均。
    let avg = one(&core, "SELECT avg(k) FILTER (WHERE g = 'a') FROM t").remove(0);
    // 3.75 = 4 行（3,1,2,9）の平均（Numeric の内部表現は scale 付き整数）。
    assert!(avg.contains("unscaled: 37500000000000000"), "{avg}");
    assert_eq!(
        one(
            &core,
            "SELECT count(s) FILTER (WHERE g = 'a'), count(DISTINCT s) FILTER (WHERE g = 'a'), \
             min(s) FILTER (WHERE g = 'b'), max(s) FILTER (WHERE g = 'b') FROM t"
        ),
        strs(&["3", "2", "w", "z"])
    );
    // FILTER の有無で別名つきの項目を並べても互いに干渉しない。
    assert_eq!(
        one(
            &core,
            "SELECT count(*) AS total, count(*) FILTER (WHERE k > 2) AS big FROM t"
        ),
        strs(&["8", "4"])
    );
}

/// `string_agg`・`array_agg`（内側の ORDER BY・DISTINCT つき）にも効く。
#[test]
fn filter_applies_to_string_agg_and_array_agg() {
    let (core, _g) = build_core("sql57-flt-aggs");
    assert_eq!(
        one(
            &core,
            "SELECT string_agg(s, ',' ORDER BY k) FILTER (WHERE g = 'a') FROM t"
        ),
        strs(&["y,x,x"])
    );
    let arr = one(
        &core,
        "SELECT array_agg(DISTINCT s) FILTER (WHERE g = 'b') FROM t",
    );
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0], r#"Array(Text([Some("w"), Some("x"), Some("z")]))"#);
    // 全行除外は NULL。
    assert_eq!(
        one(
            &core,
            "SELECT string_agg(s, ',') FILTER (WHERE g = 'none'), \
             array_agg(s) FILTER (WHERE g = 'none') FROM t"
        ),
        strs(&["NULL", "NULL"])
    );
}

/// 条件が偽・NULL の行は入れない（比較・IS NULL・NOT・OR・LIKE・IN・式述語・裸の BOOLEAN 列）。
#[test]
fn false_and_null_conditions_exclude_rows() {
    let (core, _g) = build_core("sql57-flt-cond");
    let count = |cond: &str| {
        one(
            &core,
            &format!("SELECT count(*) FILTER (WHERE {cond}) FROM t"),
        )
        .remove(0)
    };
    assert_eq!(count("k > 2"), "4");
    assert_eq!(count("k IS NULL"), "1");
    // `NOT (k > 2)` は k が NULL の行を含めない（UNKNOWN）。
    assert_eq!(count("NOT (k > 2)"), "3");
    assert_eq!(count("s LIKE 'x%'"), "3");
    assert_eq!(count("k IN (1, 2)"), "3");
    assert_eq!(count("g = 'c' OR k = 9"), "2");
    assert_eq!(count("g = 'a' AND k >= 3"), "2");
    assert_eq!(count("k + 1 > 4"), "3");
    assert_eq!(count("bo"), "4");
    assert_eq!(count("k BETWEEN 2 AND 4"), "3");
    // 常に偽。
    assert_eq!(count("g = 'none'"), "0");
}

/// 全行が除外されたとき count は 0、それ以外は NULL（`GROUP BY` なし・あり）。
#[test]
fn all_rows_excluded_yields_zero_count_and_null_others() {
    let (core, _g) = build_core("sql57-flt-empty");
    assert_eq!(
        one(
            &core,
            "SELECT count(*) FILTER (WHERE g = 'none'), sum(k) FILTER (WHERE g = 'none'), \
             min(s) FILTER (WHERE g = 'none'), count(k) FILTER (WHERE g = 'none') FROM t"
        ),
        strs(&["0", "NULL", "NULL", "0"])
    );
    // グループ自体は文の WHERE で決まり、FILTER で全除外されたグループも行として残る。
    assert_eq!(
        run(
            &core,
            "SELECT g, count(*) FILTER (WHERE k > 2) AS c, sum(k) FILTER (WHERE s = 'x') AS sx \
             FROM t GROUP BY g ORDER BY g"
        ),
        vec![
            strs(&["a", "2", "12"]),
            strs(&["b", "2", "4"]),
            strs(&["c", "0", "NULL"]),
        ]
    );
}

/// 文の WHERE と併用でき、FILTER だけが参照する列（SELECT リスト・WHERE で未参照）も正しく
/// デコードされる。繰り返し実行（visible cache・索引経路）でも結果は不変。
#[test]
fn filter_only_columns_and_repeated_execution_are_stable() {
    let (core, _g) = build_core("sql57-flt-paths");
    for _ in 0..3 {
        assert_eq!(
            one(&core, "SELECT count(*) FILTER (WHERE s = 'x') FROM t"),
            strs(&["3"])
        );
        assert_eq!(
            one(
                &core,
                "SELECT count(*) FILTER (WHERE s = 'x') FROM t WHERE g = 'a'"
            ),
            strs(&["2"])
        );
        // 索引で完全被覆されうる WHERE でも、FILTER つきの COUNT(*) は件数近道を使わない。
        assert_eq!(
            one(
                &core,
                "SELECT count(*) FILTER (WHERE k > 2), count(*) FROM t WHERE g = 'a'"
            ),
            strs(&["2", "4"])
        );
        // WHERE なしの `count(*) FILTER`（visible cache を使う Fast 経路の候補）。
        assert_eq!(
            one(&core, "SELECT count(*) FILTER (WHERE g = 'b') FROM t"),
            strs(&["3"])
        );
        // WHERE なしの単一 TEXT キー GROUP BY（キー列挙形）。
        assert_eq!(
            run(
                &core,
                "SELECT g, count(*) FILTER (WHERE s = 'x') FROM t GROUP BY g ORDER BY g"
            ),
            vec![strs(&["a", "2"]), strs(&["b", "1"]), strs(&["c", "0"]),]
        );
    }
    // embedding を参照する FILTER。
    assert_eq!(
        one(
            &core,
            "SELECT count(*) FILTER (WHERE vec_norm(embedding) > 4.5) FROM t"
        ),
        strs(&["4"])
    );
}

/// 集計式項目・HAVING・ORDER BY との組み合わせ。
#[test]
fn filter_in_expression_items_having_and_order_by() {
    let (core, _g) = build_core("sql57-flt-expr");
    assert_eq!(
        one(
            &core,
            "SELECT sum(k) FILTER (WHERE g = 'a') + 1, count(*) FILTER (WHERE g = 'b') * 2 FROM t"
        ),
        strs(&["16", "6"])
    );
    assert_eq!(
        run(
            &core,
            "SELECT g, count(*) FILTER (WHERE k > 2) AS c FROM t GROUP BY g HAVING c >= 2 ORDER BY g"
        ),
        vec![strs(&["a", "2"]), strs(&["b", "2"])]
    );
    assert_eq!(
        run(
            &core,
            "SELECT g, count(*) FILTER (WHERE k > 2) AS c FROM t GROUP BY g ORDER BY c DESC, g"
        ),
        vec![strs(&["a", "2"]), strs(&["b", "2"]), strs(&["c", "0"]),]
    );
}

/// RLS: 他テナントの Private 行は FILTER 付き集計に影響しない。自テナントからは見える。
#[test]
fn other_tenants_private_rows_never_affect_filtered_aggregates() {
    let (core, _g) = build_core("sql57-flt-rls");
    assert_eq!(
        one(
            &core,
            "SELECT count(*) FILTER (WHERE s = 'SECRET'), count(*) FILTER (WHERE k = 1000), \
             max(k) FILTER (WHERE g = 'a') FROM t"
        ),
        strs(&["0", "0", "9"])
    );
    assert_eq!(
        run_as(
            &core,
            "tenant-b",
            "SELECT count(*) FILTER (WHERE s = 'SECRET') FROM t"
        ),
        vec![strs(&["1"])]
    );
    // GROUP BY でも他テナントの値はグループにも集計にも現れない。
    assert_eq!(
        run(
            &core,
            "SELECT g, max(k) FILTER (WHERE k >= 0) FROM t GROUP BY g ORDER BY g"
        ),
        vec![strs(&["a", "9"]), strs(&["b", "5"]), strs(&["c", "1"]),]
    );
}

/// FILTER 中の部分クエリ（トップレベル経路）。内側にも他テナントの行は見えず、内側の集計は
/// FILTER 内の集計禁止（42803）の対象外。
#[test]
fn subqueries_inside_filter_are_resolved_with_the_same_rls_context() {
    let (core, _g) = build_core("sql57-flt-subq");
    assert_eq!(
        one(
            &core,
            "SELECT count(*) FILTER (WHERE g IN (SELECT g FROM t WHERE k = 9 LIMIT 10)) FROM t"
        ),
        strs(&["4"])
    );
    assert_eq!(
        one(
            &core,
            "SELECT count(*) FILTER (WHERE k = (SELECT max(k) FROM t)) FROM t"
        ),
        strs(&["1"])
    );
    // 他テナントの SECRET 行は内側からも見えない。
    assert_eq!(
        one(
            &core,
            "SELECT count(*) FILTER (WHERE s IN (SELECT s FROM t WHERE g = 'a' LIMIT 10)) FROM t"
        ),
        strs(&["4"])
    );
    assert_eq!(
        one(
            &core,
            "SELECT count(*) FILTER (WHERE EXISTS (SELECT id FROM t WHERE s = 'SECRET' LIMIT 1)) FROM t"
        ),
        strs(&["0"])
    );
    // GROUP BY でも同じ。
    assert_eq!(
        run(
            &core,
            "SELECT g, count(*) FILTER (WHERE k IN (SELECT k FROM t WHERE g = 'c' LIMIT 10)) \
             FROM t GROUP BY g ORDER BY g"
        ),
        vec![strs(&["a", "1"]), strs(&["b", "0"]), strs(&["c", "1"]),]
    );
}

/// エラー契約: 条件中の集計は 42803、ウィンドウは 42P20、構文の誤りは 42601、FROM なしと JOIN は
/// 対象外（fail-closed）。
#[test]
fn error_contract_is_fail_closed() {
    let (core, _g) = build_core("sql57-flt-err");
    assert_eq!(
        code_of(&core, "SELECT count(*) FILTER (WHERE count(*) > 1) FROM t"),
        "42803"
    );
    assert_eq!(
        code_of(
            &core,
            "SELECT sum(k) FILTER (WHERE k > (1 + max(k))) FROM t"
        ),
        "42803"
    );
    assert_eq!(
        code_of(
            &core,
            "SELECT count(*) FILTER (WHERE row_number() OVER () > 1) FROM t"
        ),
        "42P20"
    );
    assert_eq!(
        code_of(
            &core,
            "SELECT count(*) FILTER (WHERE sum(k) OVER () > 1) FROM t"
        ),
        "42P20"
    );
    // `FILTER (` の後が WHERE でない・`FILTER` だけ・条件が空。
    assert_eq!(
        code_of(&core, "SELECT count(*) FILTER (g = 'a') FROM t"),
        "42601"
    );
    assert_eq!(code_of(&core, "SELECT count(*) FILTER FROM t"), "42601");
    assert_eq!(
        code_of(&core, "SELECT count(*) FILTER (WHERE) FROM t"),
        "42601"
    );
    // 閉じ括弧の欠落。
    assert_eq!(
        code_of(&core, "SELECT count(*) FILTER (WHERE g = 'a' FROM t"),
        "42601"
    );
    // ウィンドウ集計の FILTER は受理する（Issue #1894。結果は sql64_window_filter で固定）。
    let accepted = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT count(*) FILTER (WHERE g = 'a') OVER () FROM t LIMIT 100",
        )
        .expect("window FILTER is accepted");
    assert_eq!(accepted.rows.len(), 8);
    // FROM なしの集計の FILTER は対象外。
    let const_code = code_of(&core, "SELECT count(*) FILTER (WHERE 1 = 1)");
    assert!(
        const_code == "0A000" || const_code == "42601",
        "unexpected code {const_code}"
    );
    // FILTER の文脈語は列名・別名として使える（`filter` 列の参照を壊さない）。
    assert_eq!(one(&core, "SELECT count(*) AS filter FROM t"), strs(&["8"]));
}

/// ビュー越し: 公開列に限定され（非公開列を条件で参照すると 42703）、公開列の FILTER は動く。
#[test]
fn views_expose_only_public_columns_to_filter() {
    let (core, _g) = build_core("sql57-flt-view");
    ddl(&core, "CREATE VIEW pv AS SELECT g, k FROM t");
    assert_eq!(
        one(&core, "SELECT count(*) FILTER (WHERE k > 2) FROM pv"),
        strs(&["4"])
    );
    assert_eq!(
        code_of(&core, "SELECT count(*) FILTER (WHERE s = 'x') FROM pv"),
        "42703"
    );
    assert_eq!(
        run(
            &core,
            "SELECT g, count(*) FILTER (WHERE k > 2) FROM pv GROUP BY g ORDER BY g"
        ),
        vec![strs(&["a", "2"]), strs(&["b", "2"]), strs(&["c", "0"]),]
    );
}

/// 別名つきビュー: 公開名の条件が物理名へ写されて正しく評価される。
#[test]
fn aliased_view_columns_are_mapped_to_physical_names() {
    let (core, _g) = build_core("sql57-flt-view-alias");
    ddl(&core, "CREATE VIEW av (grp, kk) AS SELECT g, k FROM t");
    assert_eq!(
        one(&core, "SELECT count(*) FILTER (WHERE kk > 2) FROM av"),
        strs(&["4"])
    );
    assert_eq!(
        one(&core, "SELECT sum(kk) FILTER (WHERE grp = 'a') FROM av"),
        strs(&["15"])
    );
}

/// カーソル経路（DECLARE の内側の集計）でも FILTER は同じ意味論。
#[test]
fn cursor_declare_with_filter_matches_direct_query() {
    let (core, _g) = build_core("sql57-flt-cursor");
    let mut session = engine::sql::mode::SessionState::default();
    let mut txn = core.new_session_transaction();
    let caller = ctx("tenant-a");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &caller,
        &mut session,
        &mut txn,
        "DECLARE c CURSOR FOR SELECT g, count(*) FILTER (WHERE k > 2) FROM t GROUP BY g ORDER BY g",
    )
    .expect("declare");
    let fetched = core
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "FETCH 10 FROM c")
        .expect("fetch");
    let engine::sql::SqlOutcome::Fetch(r) = fetched else {
        panic!("expected a query outcome");
    };
    assert_eq!(
        rows_of(&r),
        vec![strs(&["a", "2"]), strs(&["b", "2"]), strs(&["c", "0"]),]
    );
}
/// 対象外の形は fail-closed（JOIN の集計）。単純形ビュー経由の部分クエリと評価後射影形ビューでも
/// FILTER は動き、行 identity（`id`）の参照で誤集計しない。
#[test]
fn unsupported_shapes_fail_closed_and_buffered_views_work() {
    let (core, _g) = build_core("sql57-flt-misc");
    ddl(&core, "CREATE VIEW pv AS SELECT g, k FROM t");
    // 単純形ビューは基底表へ畳み込まれ、部分クエリもトップレベルの解決を経る。
    assert_eq!(
        one(
            &core,
            "SELECT count(*) FILTER (WHERE g IN (SELECT g FROM t WHERE k = 9 LIMIT 1)) FROM pv"
        ),
        strs(&["4"])
    );
    let join_code = code_of(
        &core,
        "SELECT count(*) FILTER (WHERE a.k > 1) FROM t a JOIN t b ON a.id = b.id",
    );
    assert!(
        join_code == "42601" || join_code == "0A000",
        "unexpected code {join_code}"
    );
    ddl(
        &core,
        "CREATE VIEW ev AS SELECT id, g, k FROM t ORDER BY id LIMIT 100",
    );
    assert_eq!(
        one(&core, "SELECT count(*) FILTER (WHERE k > 2) FROM ev"),
        strs(&["4"])
    );
    let id_result = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT count(*) FILTER (WHERE id > 3) FROM ev",
        )
        .map(|r| rows_of(&r));
    // 行 identity（id 4..8 の 5 行）が評価後射影形ビューの外側でも正しく FILTER へ渡る。
    assert_eq!(
        id_result.expect("id filter over buffered view"),
        vec![strs(&["5"])]
    );
}

/// HAVING 内の集計 FILTER: 部分クエリは SELECT 側と同じく解決され、ビューの非公開列は拒否される。
#[test]
fn having_aggregate_filter_resolves_subqueries_and_respects_view_columns() {
    let (core, _g) = build_core("sql57-flt-having");
    assert_eq!(
        run(
            &core,
            "SELECT g, count(*) FROM t GROUP BY g \
             HAVING count(*) FILTER (WHERE k = (SELECT max(k) FROM t)) > 0 ORDER BY g"
        ),
        vec![strs(&["a", "4"])]
    );
    ddl(&core, "CREATE VIEW pv AS SELECT g FROM t");
    assert_eq!(
        code_of(
            &core,
            "SELECT g, count(*) FROM pv GROUP BY g \
             HAVING count(*) FILTER (WHERE s = 'SECRET') > 0"
        ),
        "42703"
    );
}
