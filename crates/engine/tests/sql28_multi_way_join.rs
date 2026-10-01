//! 3 テーブル以上の連鎖 JOIN・結合での集計形・スカラー `ORDER BY`・`OR`／`IN`／
//! 列同士の比較（Issue #1190。ポインタ: SQL-28・RLS-10・TASK-212）の結合テスト。
//!
//! `tests/sql28_inner_join.rs`・`tests/sql28_outer_join.rs` と同じ流儀（実 `Storage` ＋
//! `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`、`EngineCore` を production
//! 経路として使う）。CI に PostgreSQL のオラクルは無いため、期待値は PostgreSQL と
//! 同じ意味論に従って手計算した値としてテスト内に固定している。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn dept_schema() -> TableSchema {
    TableSchema::new(
        "dept",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("dname", ColumnType::Text, false),
            ColumnDef::new("region", ColumnType::Text, true),
        ],
    )
}

fn emp_schema() -> TableSchema {
    TableSchema::new(
        "emp",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("ename", ColumnType::Text, false),
            ColumnDef::new("dept_id", ColumnType::BigInt, true),
            ColumnDef::new("salary", ColumnType::BigInt, false),
            ColumnDef::new("active", ColumnType::Boolean, false),
        ],
    )
}

fn proj_schema() -> TableSchema {
    TableSchema::new(
        "proj",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("pname", ColumnType::Text, false),
            ColumnDef::new("emp_id", ColumnType::BigInt, true),
            ColumnDef::new("budget", ColumnType::BigInt, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn vec2(id: u64) -> Value {
    Value::Vector(vec![id as f32, 0.0])
}

fn opt_i64(v: Option<i64>) -> Value {
    v.map_or(Value::Null, Value::BigInt)
}

fn insert(
    storage: &Storage,
    table: &str,
    tenant: &PolicyContext,
    id: u64,
    vis: Visibility,
    values: &[Value],
) {
    let op_id = engine::recovery::required_op_id::OperationId::parse(&format!(
        "seed-{table}-{}-{id}",
        tenant.tenant_id()
    ))
    .expect("valid operation_id");
    engine::tenant::insert_typed_row(storage, table, tenant, id, vis, values, &op_id)
        .expect("insert row");
}

fn insert_dept(
    storage: &Storage,
    tenant: &PolicyContext,
    id: u64,
    dname: &str,
    region: Option<&str>,
    vis: Visibility,
) {
    insert(
        storage,
        "dept",
        tenant,
        id,
        vis,
        &[
            vec2(id),
            Value::Text(dname.to_string()),
            region.map_or(Value::Null, |r| Value::Text(r.to_string())),
        ],
    );
}

/// `(id, ename, dept_id, salary, active)`。
type EmpRow<'a> = (u64, &'a str, Option<i64>, i64, bool);

fn insert_emp(
    storage: &Storage,
    tenant: &PolicyContext,
    (id, ename, dept_id, salary, active): EmpRow<'_>,
    vis: Visibility,
) {
    insert(
        storage,
        "emp",
        tenant,
        id,
        vis,
        &[
            vec2(id),
            Value::Text(ename.to_string()),
            opt_i64(dept_id),
            Value::BigInt(salary),
            Value::Bool(active),
        ],
    );
}

fn insert_proj(
    storage: &Storage,
    tenant: &PolicyContext,
    id: u64,
    pname: &str,
    emp_id: Option<i64>,
    budget: Option<i64>,
    vis: Visibility,
) {
    insert(
        storage,
        "proj",
        tenant,
        id,
        vis,
        &[
            vec2(id),
            Value::Text(pname.to_string()),
            opt_i64(emp_id),
            opt_i64(budget),
        ],
    );
}

/// 共通フィクスチャ（tenant-a・Public）。
///
/// dept: 1 eng/east, 2 ops/west, 3 hr/NULL
/// emp:  10 ann(dept1,100,active) 11 bob(dept1,200,active) 12 cat(dept2,150,inactive)
///       13 dan(dept NULL,50,active) 14 eve(dept 99,70,inactive)
/// proj: 20 p1(emp10,5) 21 p2(emp10,7) 22 p3(emp12,NULL) 23 p4(emp NULL,9) 24 p5(emp11,3)
fn seeded() -> (Storage, std::path::PathBuf) {
    let path = unique_db_path("multi-way-join");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&dept_schema()).expect("create dept");
    storage.create_table(&emp_schema()).expect("create emp");
    storage.create_table(&proj_schema()).expect("create proj");
    let a = ctx("tenant-a");
    let p = Visibility::Public;
    insert_dept(&storage, &a, 1, "eng", Some("east"), p);
    insert_dept(&storage, &a, 2, "ops", Some("west"), p);
    insert_dept(&storage, &a, 3, "hr", None, p);
    insert_emp(&storage, &a, (10, "ann", Some(1), 100, true), p);
    insert_emp(&storage, &a, (11, "bob", Some(1), 200, true), p);
    insert_emp(&storage, &a, (12, "cat", Some(2), 150, false), p);
    insert_emp(&storage, &a, (13, "dan", None, 50, true), p);
    insert_emp(&storage, &a, (14, "eve", Some(99), 70, false), p);
    insert_proj(&storage, &a, 20, "p1", Some(10), Some(5), p);
    insert_proj(&storage, &a, 21, "p2", Some(10), Some(7), p);
    insert_proj(&storage, &a, 22, "p3", Some(12), None, p);
    insert_proj(&storage, &a, 23, "p4", None, Some(9), p);
    insert_proj(&storage, &a, 24, "p5", Some(11), Some(3), p);
    (storage, path)
}

fn expect_query(outcome: SqlOutcome) -> QueryResult {
    match outcome {
        SqlOutcome::Query(result) => result,
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    let mut session = SessionState::default();
    expect_query(
        core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
            .unwrap_or_else(|e| panic!("query should succeed: sql={sql:?} err={e:?}")),
    )
}

fn run_err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql) {
        Ok(outcome) => panic!("expected error, got {outcome:?}: sql={sql:?}"),
        Err(e) => e,
    }
}

fn assert_rejected(core: &EngineCore, sql: &str, wire_code: &str) {
    let err = run_err(core, "tenant-a", sql);
    assert_eq!(err.wire_code(), wire_code, "sql={sql:?} err={err:?}");
}

fn render(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        Cell::Bool(b) => b.to_string(),
        Cell::Float(f) => format!("{f}"),
        other => format!("{other:?}"),
    }
}

/// 各行を `|` 区切りの文字列にする。
fn rows(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(render).collect::<Vec<_>>().join("|"))
        .collect()
}

fn q(core: &EngineCore, sql: &str) -> Vec<String> {
    rows(&run(core, "tenant-a", sql))
}

// ---------- N 方向連鎖 ----------

#[test]
fn three_way_inner_join_returns_the_matching_chain_in_scan_order() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, emp.ename, proj.pname FROM dept \
             JOIN emp ON emp.dept_id = dept.id \
             JOIN proj ON proj.emp_id = emp.id LIMIT 100"
        ),
        vec!["eng|ann|p1", "eng|ann|p2", "eng|bob|p5", "ops|cat|p3"]
    );
}

#[test]
fn left_chain_keeps_unmatched_rows_with_null_padding_at_every_level() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, emp.ename, proj.pname FROM dept \
             LEFT JOIN emp ON emp.dept_id = dept.id \
             LEFT JOIN proj ON proj.emp_id = emp.id LIMIT 100"
        ),
        vec![
            "eng|ann|p1",
            "eng|ann|p2",
            "eng|bob|p5",
            "ops|cat|p3",
            "hr|NULL|NULL"
        ]
    );
}

#[test]
fn full_join_keeps_both_unmatched_sides() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename, dept.dname FROM emp FULL JOIN dept ON emp.dept_id = dept.id LIMIT 100"
        ),
        vec![
            "ann|eng",
            "bob|eng",
            "cat|ops",
            "dan|NULL",
            "eve|NULL",
            "NULL|hr"
        ]
    );
}

#[test]
fn inner_then_right_chain_preserves_the_new_relation_only() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, emp.ename, proj.pname FROM dept \
             JOIN emp ON emp.dept_id = dept.id \
             RIGHT JOIN proj ON proj.emp_id = emp.id LIMIT 100"
        ),
        vec![
            "eng|ann|p1",
            "eng|ann|p2",
            "eng|bob|p5",
            "ops|cat|p3",
            "NULL|NULL|p4"
        ]
    );
}

#[test]
fn four_and_more_tables_and_three_way_self_join_are_accepted() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    // 4 テーブル（dept 自己結合を含む）。
    assert_eq!(
        q(
            &core,
            "SELECT proj.pname, d2.dname FROM proj \
             JOIN emp ON proj.emp_id = emp.id \
             JOIN dept d1 ON emp.dept_id = d1.id \
             JOIN dept d2 ON d1.id = d2.id LIMIT 100"
        ),
        vec!["p1|eng", "p2|eng", "p3|ops", "p5|eng"]
    );
    // 3 方向の自己結合（別名違い）。
    assert_eq!(
        q(
            &core,
            "SELECT a.ename, b.ename, c.ename FROM emp a \
             JOIN emp b ON a.dept_id = b.dept_id \
             JOIN emp c ON b.dept_id = c.dept_id \
             WHERE a.ename = 'ann' AND c.ename = 'bob' LIMIT 100"
        ),
        vec!["ann|ann|bob", "ann|bob|bob"]
    );
    // relation 数の上限（8）ちょうどは受理し、9 は 54000。
    let chain = |n: usize| {
        let mut sql = String::from("SELECT t0.dname FROM dept t0");
        for i in 1..n {
            sql.push_str(&format!(" JOIN dept t{i} ON t{i}.id = t0.id"));
        }
        sql.push_str(" LIMIT 10");
        sql
    };
    assert_eq!(q(&core, &chain(8)), vec!["eng", "ops", "hr"]);
    assert_rejected(&core, &chain(9), "54000");
}

#[test]
fn on_clause_scoping_rejects_forward_reference_and_both_earlier_sides() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    // 未出現の relation（proj）を ON で前方参照 → 42P01。
    assert_rejected(
        &core,
        "SELECT * FROM dept JOIN emp ON emp.dept_id = proj.id JOIN proj ON proj.emp_id = emp.id LIMIT 10",
        "42P01",
    );
    // 両側とも既出 relation → 42601。
    assert_rejected(
        &core,
        "SELECT * FROM dept JOIN emp ON emp.dept_id = dept.id JOIN proj ON emp.id = dept.id LIMIT 10",
        "42601",
    );
    // 結合キーの型不一致は段ごとに 42804。
    assert_rejected(
        &core,
        "SELECT * FROM dept JOIN emp ON emp.dept_id = dept.id JOIN proj ON proj.pname = emp.id LIMIT 10",
        "42804",
    );
}

// ---------- WHERE（OR・IN・列同士の比較・簡約） ----------

#[test]
fn where_on_missing_side_reduces_outer_join_and_where_on_preserved_side_keeps_padding() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    // 欠損側（emp）への strict な述語 → INNER に簡約。
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, emp.ename FROM dept LEFT JOIN emp ON emp.dept_id = dept.id \
             WHERE emp.ename = 'ann' LIMIT 10"
        ),
        vec!["eng|ann"]
    );
    // 保存側（dept）への述語 → NULL 補完行を保つ。
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, emp.ename FROM dept LEFT JOIN emp ON emp.dept_id = dept.id \
             WHERE dept.dname = 'hr' LIMIT 10"
        ),
        vec!["hr|NULL"]
    );
    // 連鎖の途中の relation への述語（emp）は段 0 を INNER に簡約するが、
    // 段 1（proj への LEFT）は保つ。
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, emp.ename, proj.pname FROM dept \
             LEFT JOIN emp ON emp.dept_id = dept.id \
             LEFT JOIN proj ON proj.emp_id = emp.id \
             WHERE emp.ename IN ('bob', 'cat', 'dan') LIMIT 10"
        ),
        vec!["eng|bob|p5", "ops|cat|p3"]
    );
}

#[test]
fn full_join_with_predicate_on_one_side_becomes_one_sided_outer_join() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    // emp への述語 → FULL は「emp を保存する LEFT」になり、dept のみの行 hr は落ちる。
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename, dept.dname FROM emp FULL JOIN dept ON emp.dept_id = dept.id \
             WHERE emp.ename >= 'b' LIMIT 100"
        ),
        vec!["bob|eng", "cat|ops", "dan|NULL", "eve|NULL"]
    );
}

#[test]
fn single_relation_or_and_in_are_pushed_down() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id \
             WHERE emp.ename = 'ann' OR emp.ename IN ('cat', 'zzz') LIMIT 10"
        ),
        vec!["ann", "cat"]
    );
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id \
             WHERE (emp.ename = 'ann' OR (emp.ename = 'bob' AND dept.dname = 'eng')) LIMIT 10"
        ),
        vec!["ann", "bob"]
    );
}

#[test]
fn or_across_relations_is_evaluated_after_the_join_with_null_padded_rows_false() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    // cat の行は左枝が真、hr の NULL 補完行は右枝（dept.dname = 'hr'）が真で残る。
    // それ以外の NULL 補完行は無いが、eng の行は両枝とも偽で落ちる。
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, emp.ename FROM dept LEFT JOIN emp ON emp.dept_id = dept.id \
             WHERE emp.ename = 'cat' OR dept.dname = 'hr' LIMIT 10"
        ),
        vec!["ops|cat", "hr|NULL"]
    );
    // 左枝は NULL 補完された emp を参照するため、hr の行は右枝が偽なら落ちる。
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, emp.ename FROM dept LEFT JOIN emp ON emp.dept_id = dept.id \
             WHERE emp.ename = 'cat' OR dept.dname = 'eng' LIMIT 10"
        ),
        vec!["eng|ann", "eng|bob", "ops|cat"]
    );
}

#[test]
fn column_to_column_comparison_across_and_within_relations() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    // relation 間: NULL（p3 の budget）との比較は偽。
    assert_eq!(
        q(
            &core,
            "SELECT proj.pname FROM emp JOIN proj ON proj.emp_id = emp.id \
             WHERE emp.salary > proj.budget LIMIT 10"
        ),
        vec!["p1", "p2", "p5"]
    );
    // 整数クラスの異種比較（疑似列 id と BIGINT）。
    assert_eq!(
        q(
            &core,
            "SELECT proj.pname FROM emp JOIN proj ON proj.emp_id = emp.id \
             WHERE emp.id > proj.budget LIMIT 10"
        ),
        vec!["p1", "p2", "p5"]
    );
    // 同一 relation 内。
    assert_eq!(
        q(
            &core,
            "SELECT proj.pname FROM proj JOIN emp ON proj.emp_id = emp.id \
             WHERE proj.budget < proj.emp_id LIMIT 10"
        ),
        vec!["p1", "p2", "p5"]
    );
    // TEXT 同士の比較。
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id \
             WHERE emp.ename < dept.dname LIMIT 10"
        ),
        vec!["ann", "bob", "cat"]
    );
    // クラス不一致は 42804。
    assert_rejected(
        &core,
        "SELECT * FROM emp JOIN dept ON emp.dept_id = dept.id WHERE emp.ename = dept.id LIMIT 10",
        "42804",
    );
    // VECTOR 列同士の比較は 42601。
    assert_rejected(
        &core,
        "SELECT * FROM emp JOIN dept ON emp.dept_id = dept.id WHERE emp.embedding = dept.embedding LIMIT 10",
        "42601",
    );
}

#[test]
fn alias_reserved_words_and_bool_column_terminators_are_parsed() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    // ORDER／GROUP 等を別名として誤消費しない。
    assert_eq!(
        q(
            &core,
            "SELECT e.ename FROM dept d JOIN emp e ON e.dept_id = d.id ORDER BY e.salary DESC LIMIT 5"
        ),
        vec!["bob", "cat", "ann"]
    );
    // bool 列の直後に OR／`)`／ORDER が来る形。
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id \
             WHERE emp.active OR dept.dname = 'ops' LIMIT 10"
        ),
        vec!["ann", "bob", "cat"]
    );
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id \
             WHERE (emp.active) AND emp.ename = 'ann' LIMIT 10"
        ),
        vec!["ann"]
    );
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id \
             WHERE emp.active ORDER BY emp.salary DESC LIMIT 10"
        ),
        vec!["bob", "ann"]
    );
}

#[test]
fn unsupported_where_forms_and_other_shapes_stay_rejected() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    let base = "SELECT * FROM emp JOIN dept ON emp.dept_id = dept.id";
    for tail in [
        "WHERE NOT emp.active LIMIT 10",
        "WHERE emp.ename IS NULL LIMIT 10",
        "WHERE emp.salary BETWEEN '1' AND '9' LIMIT 10",
        "WHERE emp.ename <> 'ann' LIMIT 10",
        "WHERE emp.ename NOT IN ('ann') LIMIT 10",
        "WHERE emp.ename = 'ann' ORDER BY emp.embedding <=> '[0.1,0.1]' LIMIT 10",
        "ORDER BY emp.salary",
        "",
    ] {
        assert_rejected(&core, &format!("{base} {tail}"), "42601");
    }
    for sql in [
        "SELECT COUNT(DISTINCT emp.ename) FROM emp JOIN dept ON emp.dept_id = dept.id",
        "SELECT SUM(emp.salary * 2) FROM emp JOIN dept ON emp.dept_id = dept.id",
        "SELECT COUNT(emp.embedding) FROM emp JOIN dept ON emp.dept_id = dept.id",
        "SELECT dept.dname, COUNT(*) FROM emp JOIN dept ON emp.dept_id = dept.id",
        "SELECT dept.region, COUNT(*) FROM emp JOIN dept ON emp.dept_id = dept.id GROUP BY dept.dname",
        "SELECT COUNT(*) FROM emp JOIN dept ON emp.dept_id = dept.id ORDER BY emp.embedding <=> '[0.1,0.1]'",
    ] {
        assert_rejected(&core, sql, "42601");
    }
}

// ---------- スカラー ORDER BY ----------

#[test]
fn scalar_order_by_handles_direction_nulls_unprojected_columns_and_paging() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    // 投影していない列でのソート。
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id \
             ORDER BY emp.salary DESC LIMIT 10"
        ),
        vec!["bob", "cat", "ann"]
    );
    // 複数キー。
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id \
             ORDER BY dept.dname DESC, emp.ename ASC LIMIT 10"
        ),
        vec!["cat", "ann", "bob"]
    );
    // 同値キーは結合直後の決定的順序を保つ（安定ソート）。
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id \
             ORDER BY dept.dname LIMIT 10"
        ),
        vec!["ann", "bob", "cat"]
    );
    // NULL 位置は ASC で末尾・DESC で先頭。
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, emp.ename FROM dept LEFT JOIN emp ON emp.dept_id = dept.id \
             ORDER BY emp.ename ASC LIMIT 10"
        ),
        vec!["eng|ann", "eng|bob", "ops|cat", "hr|NULL"]
    );
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, emp.ename FROM dept LEFT JOIN emp ON emp.dept_id = dept.id \
             ORDER BY emp.ename DESC LIMIT 10"
        ),
        vec!["hr|NULL", "ops|cat", "eng|bob", "eng|ann"]
    );
    // OFFSET／LIMIT はソート後に適用する。
    assert_eq!(
        q(
            &core,
            "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id \
             ORDER BY emp.salary DESC LIMIT 1 OFFSET 1"
        ),
        vec!["cat"]
    );
    // VECTOR 列は並べ替えキーにできない。
    assert_rejected(
        &core,
        "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id ORDER BY emp.embedding LIMIT 10",
        "22000",
    );
}

// ---------- 集計形 ----------

#[test]
fn aggregate_counts_distinguish_star_from_column_over_null_padded_rows() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    assert_eq!(
        q(
            &core,
            "SELECT COUNT(*), COUNT(emp.id) FROM dept LEFT JOIN emp ON emp.dept_id = dept.id"
        ),
        vec!["4|3"]
    );
}

#[test]
fn group_by_with_aggregates_having_and_order_by() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    let base = "FROM dept LEFT JOIN emp ON emp.dept_id = dept.id";
    assert_eq!(
        q(
            &core,
            &format!(
                "SELECT dept.dname, COUNT(emp.id) AS n, SUM(emp.salary) AS total {base} \
                 GROUP BY dept.dname ORDER BY dept.dname"
            )
        ),
        vec!["eng|2|300", "hr|0|NULL", "ops|1|150"]
    );
    // HAVING は集計項目名を参照する。
    assert_eq!(
        q(
            &core,
            &format!(
                "SELECT dept.dname, COUNT(emp.id) AS n {base} \
                 GROUP BY dept.dname HAVING n > 1"
            )
        ),
        vec!["eng|2"]
    );
    // 集計項目名での ORDER BY（DESC は NULL 先頭）と LIMIT／OFFSET。
    assert_eq!(
        q(
            &core,
            &format!(
                "SELECT dept.dname, SUM(emp.salary) AS total {base} \
                 GROUP BY dept.dname ORDER BY total DESC"
            )
        ),
        vec!["hr|NULL", "eng|300", "ops|150"]
    );
    assert_eq!(
        q(
            &core,
            &format!(
                "SELECT dept.dname, COUNT(*) AS n {base} \
                 GROUP BY dept.dname ORDER BY dept.dname LIMIT 1 OFFSET 1"
            )
        ),
        vec!["hr|1"]
    );
    // TEXT の MIN／MAX（NULL 補完行は観測しない）。
    assert_eq!(
        q(
            &core,
            &format!(
                "SELECT dept.dname, MIN(emp.ename) AS lo, MAX(emp.ename) AS hi {base} \
                 GROUP BY dept.dname ORDER BY dept.dname"
            )
        ),
        vec!["eng|ann|bob", "hr|NULL|NULL", "ops|cat|cat"]
    );
    // AVG（整数入力の結果は浮動小数）。
    assert_eq!(
        q(
            &core,
            &format!(
                "SELECT dept.dname, AVG(emp.salary) AS mean {base} \
                 GROUP BY dept.dname ORDER BY dept.dname"
            )
        ),
        vec!["eng|150", "hr|NULL", "ops|150"]
    );
}

#[test]
fn group_by_null_key_forms_one_group_and_multi_key_grouping_works() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    // emp.dept_id の NULL は 1 グループ（ASC で末尾）。
    assert_eq!(
        q(
            &core,
            "SELECT emp.dept_id, COUNT(*) AS n FROM emp LEFT JOIN proj ON proj.emp_id = emp.id \
             GROUP BY emp.dept_id ORDER BY emp.dept_id"
        ),
        vec!["1|3", "2|1", "99|1", "NULL|1"]
    );
    // 複数キー。
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, emp.ename, COUNT(proj.id) AS pc FROM dept \
             JOIN emp ON emp.dept_id = dept.id JOIN proj ON proj.emp_id = emp.id \
             GROUP BY dept.dname, emp.ename ORDER BY dept.dname, emp.ename"
        ),
        vec!["eng|ann|2", "eng|bob|1", "ops|cat|1"]
    );
    // 集計項目を持たない GROUP BY（重複排除）。
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname FROM dept JOIN emp ON emp.dept_id = dept.id \
             GROUP BY dept.dname ORDER BY dept.dname"
        ),
        vec!["eng", "ops"]
    );
}

#[test]
fn aggregate_over_empty_join_returns_one_row_without_group_by_and_none_with_it() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    assert_eq!(
        q(
            &core,
            "SELECT COUNT(*), SUM(emp.salary) FROM emp JOIN dept ON emp.dept_id = dept.id \
             WHERE emp.ename = 'zzz'"
        ),
        vec!["0|NULL"]
    );
    assert!(q(
        &core,
        "SELECT dept.dname, COUNT(*) FROM emp JOIN dept ON emp.dept_id = dept.id \
         WHERE emp.ename = 'zzz' GROUP BY dept.dname"
    )
    .is_empty());
}

#[test]
fn aggregate_on_a_three_way_chain_and_residual_where() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    assert_eq!(
        q(
            &core,
            "SELECT dept.dname, COUNT(proj.id) AS pc, SUM(proj.budget) AS total FROM dept \
             JOIN emp ON emp.dept_id = dept.id JOIN proj ON proj.emp_id = emp.id \
             GROUP BY dept.dname ORDER BY dept.dname"
        ),
        vec!["eng|3|15", "ops|1|NULL"]
    );
    // 残余 WHERE（列同士の比較）と集計の組み合わせ。
    assert_eq!(
        q(
            &core,
            "SELECT COUNT(*) FROM emp JOIN proj ON proj.emp_id = emp.id \
             WHERE emp.salary > proj.budget"
        ),
        vec!["3"]
    );
}

#[test]
fn aggregate_type_and_name_errors_use_the_single_table_classification() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    let base = "FROM dept JOIN emp ON emp.dept_id = dept.id";
    // TEXT 列への SUM は型不整合（単一テーブル経路と同じ 22000）。
    assert_rejected(&core, &format!("SELECT SUM(emp.ename) {base}"), "22000");
    // HAVING の対象が数値として比較できない集計結果。
    assert_rejected(
        &core,
        &format!("SELECT dept.dname, MIN(emp.ename) AS m {base} GROUP BY dept.dname HAVING m > 1"),
        "22000",
    );
    // HAVING の対象が集計項目名でない。
    assert_rejected(
        &core,
        &format!("SELECT dept.dname, COUNT(*) AS n {base} GROUP BY dept.dname HAVING zz > 1"),
        "22000",
    );
    // 同名の集計項目が複数ある別名は、ORDER BY・HAVING とも曖昧として拒否する
    // （先頭の集計値へ黙って解決しない。単一テーブル集計と同じ 42702。Issue #1270）。
    assert_rejected(
        &core,
        &format!(
            "SELECT dept.dname, SUM(emp.salary) AS total, SUM(emp.id) AS total {base} \
             GROUP BY dept.dname ORDER BY total"
        ),
        "42702",
    );
    assert_rejected(
        &core,
        &format!(
            "SELECT dept.dname, SUM(emp.salary) AS total, SUM(emp.id) AS total {base} \
             GROUP BY dept.dname HAVING total > 0"
        ),
        "42702",
    );
    // ORDER BY の対象がキーにも集計項目にも一致しない／両方に一致する。
    assert_rejected(
        &core,
        &format!("SELECT dept.dname, COUNT(*) AS n {base} GROUP BY dept.dname ORDER BY zz"),
        "22000",
    );
    assert_rejected(
        &core,
        &format!("SELECT dept.dname, COUNT(*) AS dname {base} GROUP BY dept.dname ORDER BY dname"),
        "42702",
    );
    // HAVING でもキーの出力名と集計項目名が衝突すれば曖昧（ORDER BY と同じ 42702。Issue #1270）。
    assert_rejected(
        &core,
        &format!(
            "SELECT dept.dname, COUNT(*) AS dname {base} GROUP BY dept.dname HAVING dname > 0"
        ),
        "42702",
    );
    // キーの別名と集計項目名の衝突も、HAVING・ORDER BY の双方で曖昧。
    assert_rejected(
        &core,
        &format!("SELECT dept.dname AS k, COUNT(*) AS k {base} GROUP BY dept.dname HAVING k > 0"),
        "42702",
    );
    assert_rejected(
        &core,
        &format!("SELECT dept.dname AS k, COUNT(*) AS k {base} GROUP BY dept.dname ORDER BY k"),
        "42702",
    );
    // キー別名の重複も曖昧（42702）。
    assert_rejected(
        &core,
        &format!(
            "SELECT dept.dname AS k, emp.ename AS k, COUNT(*) AS n {base} \
             GROUP BY dept.dname, emp.ename ORDER BY k"
        ),
        "42702",
    );
    // GROUP BY に使えない型（VECTOR）。
    assert_rejected(
        &core,
        &format!("SELECT dept.embedding, COUNT(*) {base} GROUP BY dept.embedding"),
        "22000",
    );
}

#[test]
fn aggregate_order_by_resolves_every_alias_of_a_group_key_and_propagates_bind_errors() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    let base = "FROM dept LEFT JOIN emp ON emp.dept_id = dept.id";
    // 同一 GROUP BY 列を複数の別名で SELECT しても、どちらの別名でも ORDER BY できる。
    for target in ["a", "b", "dept.dname"] {
        assert_eq!(
            q(
                &core,
                &format!(
                    "SELECT dept.dname AS a, dept.dname AS b, COUNT(*) AS n {base} \
                     GROUP BY dept.dname ORDER BY {target}"
                )
            ),
            vec!["eng|eng|2", "hr|hr|1", "ops|ops|1"],
            "target={target}"
        );
    }
    // 未知の修飾子は 42P01、曖昧な非修飾列は 42702 のまま伝播する。
    assert_rejected(
        &core,
        &format!("SELECT dept.dname, COUNT(*) AS n {base} GROUP BY dept.dname ORDER BY zz.dname"),
        "42P01",
    );
    assert_rejected(
        &core,
        &format!("SELECT dept.dname, COUNT(*) AS n {base} GROUP BY dept.dname ORDER BY id"),
        "42702",
    );
}

#[test]
fn non_aggregate_join_projection_is_not_limited_by_the_aggregate_item_cap() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    let n = engine::sql::allowlist::MAX_AGGREGATE_ITEMS + 8;
    let cols = std::iter::repeat_n("emp.ename", n)
        .collect::<Vec<_>>()
        .join(", ");
    let result = run(
        &core,
        "tenant-a",
        &format!("SELECT {cols} FROM dept JOIN emp ON emp.dept_id = dept.id LIMIT 10"),
    );
    assert_eq!(result.columns.len(), n);
}

// ---------- 上限 ----------

#[test]
fn where_tree_limits_are_enforced_before_allocation() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    let base = "SELECT * FROM emp JOIN dept ON emp.dept_id = dept.id WHERE";
    // 葉の総数。
    let leaves = (0..300)
        .map(|_| "emp.ename = 'ann'")
        .collect::<Vec<_>>()
        .join(" OR ");
    assert_rejected(&core, &format!("{base} {leaves} LIMIT 10"), "54000");
    // 括弧の深さ。
    let deep = format!("{}emp.active{}", "(".repeat(200), ")".repeat(200));
    assert_rejected(&core, &format!("{base} {deep} LIMIT 10"), "54000");
    // IN の要素数。
    let items = (0..5000)
        .map(|i| format!("'v{i}'"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_rejected(
        &core,
        &format!("{base} emp.ename IN ({items}) LIMIT 10"),
        "54000",
    );
}

// ---------- Describe ----------

#[test]
fn describe_matches_execute_columns_for_every_shape() {
    let (storage, path) = seeded();
    let _g = CleanupGuard(path);
    let core = new_core(storage);
    for sql in [
        "SELECT emp.ename, dept.dname, proj.pname FROM dept JOIN emp ON emp.dept_id = dept.id JOIN proj ON proj.emp_id = emp.id LIMIT 10",
        "SELECT * FROM dept JOIN emp ON emp.dept_id = dept.id JOIN proj ON proj.emp_id = emp.id LIMIT 10",
        "SELECT emp.ename FROM emp JOIN dept ON emp.dept_id = dept.id ORDER BY emp.salary DESC LIMIT 10",
        "SELECT dept.dname, COUNT(*) AS n, SUM(emp.salary) AS total, MIN(emp.ename) FROM dept LEFT JOIN emp ON emp.dept_id = dept.id GROUP BY dept.dname ORDER BY dept.dname",
        "SELECT COUNT(*) FROM dept JOIN emp ON emp.dept_id = dept.id",
    ] {
        let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
        let described = core
            .describe_parsed_in_session(&SessionState::default(), &parsed)
            .expect("describe should succeed")
            .expect("JOIN must produce result columns");
        let executed = run(&core, "tenant-a", sql);
        assert_eq!(described, executed.columns, "sql={sql}");
    }
    // 型不一致・比較クラス不一致は Describe でも Execute と同じエラーになる。
    for (sql, code) in [
        (
            "SELECT * FROM dept JOIN emp ON emp.dept_id = dept.id JOIN proj ON proj.pname = emp.id LIMIT 10",
            "42804",
        ),
        (
            "SELECT * FROM emp JOIN dept ON emp.dept_id = dept.id WHERE emp.ename = dept.id LIMIT 10",
            "42804",
        ),
    ] {
        let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
        let err = core
            .describe_parsed_in_session(&SessionState::default(), &parsed)
            .expect_err("describe must reject like execute");
        assert_eq!(err.wire_code(), code, "sql={sql}");
    }
}

// ---------- RLS（AC2） ----------

/// 3 テナント × 3 relation。他テナント（Private）の行を大量に投入しても、結果・
/// NULL 補完の件数・集計値・グループ数が不変であること。
#[test]
fn other_tenants_rows_never_leak_into_multi_way_results_or_aggregates() {
    let path = unique_db_path("multi-way-join-rls");
    let storage = Storage::open(&path).expect("open storage");
    let _g = CleanupGuard(path);
    storage.create_table(&dept_schema()).expect("create dept");
    storage.create_table(&emp_schema()).expect("create emp");
    storage.create_table(&proj_schema()).expect("create proj");
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");
    let c = ctx("tenant-c");
    let private = Visibility::Private;

    // tenant-a: 1 チェーン + 孤立した dept 1 件。
    insert_dept(&storage, &a, 1, "eng", None, private);
    insert_dept(&storage, &a, 2, "lonely", None, private);
    insert_emp(&storage, &a, (10, "ann", Some(1), 100, true), private);
    insert_proj(&storage, &a, 20, "p1", Some(10), Some(5), private);

    // tenant-b / tenant-c: 同じキー（dept 1・emp 10）を指す行を大量に投入する。
    for (t, base) in [(&b, 1000u64), (&c, 5000u64)] {
        insert_dept(&storage, t, 1, "evil-dept", None, private);
        insert_dept(&storage, t, 3, "evil-extra", None, private);
        for i in 0..15u64 {
            insert_emp(
                &storage,
                t,
                (base + i, "evil-emp", Some(1), 999, true),
                private,
            );
            insert_proj(
                &storage,
                t,
                base + 100 + i,
                "evil-proj",
                Some((base + i) as i64),
                Some(1000),
                private,
            );
        }
        insert_proj(
            &storage,
            t,
            base + 500,
            "evil-p10",
            Some(10),
            Some(777),
            private,
        );
    }

    let core = new_core(storage);
    let chain = "FROM dept LEFT JOIN emp ON emp.dept_id = dept.id \
                 LEFT JOIN proj ON proj.emp_id = emp.id";
    assert_eq!(
        rows(&run(
            &core,
            "tenant-a",
            &format!("SELECT dept.dname, emp.ename, proj.pname {chain} LIMIT 100")
        )),
        vec!["eng|ann|p1", "lonely|NULL|NULL"]
    );
    assert_eq!(
        rows(&run(
            &core,
            "tenant-a",
            &format!("SELECT COUNT(*), COUNT(proj.id), SUM(proj.budget) {chain}")
        )),
        vec!["2|1|5"]
    );
    assert_eq!(
        rows(&run(
            &core,
            "tenant-a",
            &format!(
                "SELECT dept.dname, COUNT(*) AS n {chain} GROUP BY dept.dname ORDER BY dept.dname"
            )
        )),
        vec!["eng|1", "lonely|1"]
    );
    // 3 方向の自己結合でも参照ごとに独立に RLS が効く。
    assert_eq!(
        rows(&run(
            &core,
            "tenant-a",
            "SELECT x.ename, y.ename, z.ename FROM emp x JOIN emp y ON x.dept_id = y.dept_id \
             JOIN emp z ON y.dept_id = z.dept_id LIMIT 100"
        )),
        vec!["ann|ann|ann"]
    );
    // tenant-b のセッションは自分の行だけを見る（件数はチェーン 15 本）。
    let b_result = run(
        &core,
        "tenant-b",
        "SELECT COUNT(*) FROM dept JOIN emp ON emp.dept_id = dept.id \
         JOIN proj ON proj.emp_id = emp.id",
    );
    assert_eq!(rows(&b_result), vec!["15"]);
}
