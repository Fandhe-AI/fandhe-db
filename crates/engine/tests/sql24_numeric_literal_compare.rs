//! NUMERIC 列 × 裸の数値リテラルの比較と、負の数値リテラルの結合テスト
//! （Issue #1430。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-24・
//! `docs/spec/04-behavior/errors.md` ERR-2）。
//!
//! NUMERIC 列の `=`・範囲比較・`IN`・`BETWEEN`（`NOT` 付き含む）が文字列リテラル形と
//! 同じ行集合・同じ `wire_code` になること、負の数値リテラルが全数値型で受理されること、
//! テナント境界が保たれること、構文の拒否が fail-closed のままであることを、実 `Storage`＋
//! `CpuScalarProvider` の production 経路（`EngineCore::execute_sql`）で固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
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
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("big", ColumnType::BigInt, true),
            ColumnDef::new("r", ColumnType::Real, true),
            ColumnDef::new("d", ColumnType::Double, true),
            ColumnDef::new(
                "price",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql24-numeric-literal-compare");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .unwrap_or_else(|e| panic!("{sql:?} should succeed, got {e:?}"));
}

fn insert(core: &EngineCore, ctx: &PolicyContext, id: u64, extra: &[(&str, &str)]) {
    let mut columns = vec!["id".to_string(), "embedding".into()];
    let mut values = vec![id.to_string(), "'[0.1,0.2]'".into()];
    for (c, v) in extra {
        columns.push((*c).to_string());
        values.push((*v).to_string());
    }
    exec(
        core,
        ctx,
        &format!(
            "INSERT INTO {TABLE} ({}) VALUES ({}) USING OPERATION_ID 'op-{id}'",
            columns.join(", "),
            values.join(", ")
        ),
    );
}

fn seed(core: &EngineCore, alice: &PolicyContext, bob: &PolicyContext) {
    let rows: [(u64, [&str; 5]); 6] = [
        (1, ["1", "1", "1.5", "1.5", "1.00"]),
        (2, ["2", "2", "2.5", "2.5", "2.50"]),
        (4, ["-1", "-1", "-1.5", "-1.5", "-1.00"]),
        (5, ["-3", "-3", "-3.5", "-3.5", "-3.00"]),
        (6, ["0", "0", "0", "0", "0.00"]),
        (7, ["100", "100", "100", "100", "7.01"]),
    ];
    for (id, v) in rows {
        insert(
            core,
            alice,
            id,
            &[
                ("qty", v[0]),
                ("big", v[1]),
                ("r", v[2]),
                ("d", v[3]),
                ("price", v[4]),
            ],
        );
    }
    // 全数値列 NULL の行（三値論理の fail-open 検出用）。
    insert(core, alice, 3, &[]);
    // 他テナントの行。
    insert(
        core,
        bob,
        9,
        &[("qty", "1"), ("big", "1"), ("price", "1.00")],
    );
}

fn ids(core: &EngineCore, ctx: &PolicyContext, where_clause: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
        )
        .unwrap_or_else(|e| panic!("query {where_clause:?} should succeed, got {e:?}"));
    let mut out: Vec<u64> = result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Integer(v)) => *v,
            other => panic!("expected Cell::Integer for id, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

fn code(core: &EngineCore, ctx: &PolicyContext, where_clause: &str) -> String {
    core.execute_sql(
        ctx,
        &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
    )
    .expect_err(&format!("query {where_clause:?} must be rejected"))
    .wire_code()
    .to_string()
}

#[test]
fn numeric_bare_literal_matches_string_literal_form() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    let pairs = [
        ("price = 1", "price = '1'"),
        ("price = 1.00", "price = '1.00'"),
        ("price < 2.5", "price < '2.5'"),
        ("price <= 2.5", "price <= '2.5'"),
        ("price > -1", "price > '-1'"),
        ("price >= -1.00", "price >= '-1.00'"),
        ("1 < price", "price > '1'"),
        ("2.5 >= price", "price <= '2.5'"),
        ("price IN (1, 2.5, -3)", "price IN ('1', '2.5', '-3')"),
        ("price NOT IN (1, 2.5)", "price NOT IN ('1', '2.5')"),
        ("price BETWEEN -2 AND 2.5", "price BETWEEN '-2' AND '2.5'"),
        (
            "price NOT BETWEEN -2 AND 2.5",
            "price NOT BETWEEN '-2' AND '2.5'",
        ),
        ("NOT price = 1", "NOT price = '1'"),
        ("price = 1 OR price = -3", "price = '1' OR price = '-3'"),
        ("price = 1e2", "price = '1e2'"),
        ("price > 1.5e-1", "price > '1.5e-1'"),
        ("price = 7.005", "price = '7.005'"),
        ("price >= 7.005", "price >= '7.005'"),
        ("price = 1.", "price = '1.'"),
    ];
    for (numeric, quoted) in pairs {
        assert_eq!(
            ids(&core, &alice, numeric),
            ids(&core, &alice, quoted),
            "{numeric} vs {quoted}"
        );
    }
    assert_eq!(ids(&core, &alice, "price = 1"), vec![1]);
    assert_eq!(
        ids(&core, &alice, "price BETWEEN -2 AND 2.5"),
        vec![1, 2, 4, 6]
    );
    // NULL 行（id 3）は NOT 付きでも除外される。
    assert!(!ids(&core, &alice, "price NOT IN (1, 2.5)").contains(&3));
    assert!(!ids(&core, &alice, "price NOT BETWEEN -2 AND 2.5").contains(&3));
    // 他テナント行（id 9）は現れない。
    assert!(!ids(&core, &alice, "price = 1").contains(&9));
}

#[test]
fn numeric_out_of_range_literal_has_same_wire_code_as_string_form() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    let huge = "1".to_string() + &"0".repeat(40);
    let numeric = code(&core, &alice, &format!("price = {huge}"));
    let quoted = code(&core, &alice, &format!("price = '{huge}'"));
    assert_eq!(numeric, quoted);
}

#[test]
fn negative_literals_accepted_for_every_numeric_type() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    for col in ["qty", "big", "r", "d", "price"] {
        let pairs = [
            (format!("{col} = -1"), format!("{col} = '-1'")),
            (format!("{col} < -1"), format!("{col} < '-1'")),
            (format!("{col} IN (-1, 2)"), format!("{col} IN ('-1', '2')")),
            (format!("{col} NOT IN (-1)"), format!("{col} NOT IN ('-1')")),
            (
                format!("{col} BETWEEN -2 AND 2"),
                format!("{col} BETWEEN '-2' AND '2'"),
            ),
            (
                format!("{col} NOT BETWEEN -2 AND -1"),
                format!("{col} NOT BETWEEN '-2' AND '-1'"),
            ),
            (format!("-1 = {col}"), format!("{col} = '-1'")),
        ];
        for (numeric, quoted) in pairs {
            assert_eq!(
                ids(&core, &alice, &numeric),
                ids(&core, &alice, &quoted),
                "{numeric}"
            );
        }
    }
    assert_eq!(ids(&core, &alice, "qty = -1"), vec![4]);
    assert_eq!(ids(&core, &alice, "qty IN (-1, -3)"), vec![4, 5]);
    assert_eq!(ids(&core, &alice, "qty BETWEEN -3 AND -1"), vec![4, 5]);
    // 空白を挟んでも受理される。
    assert_eq!(ids(&core, &alice, "qty = - 1"), vec![4]);
}

#[test]
fn negative_id_pseudo_column_is_accepted_and_matches_nothing() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    assert!(ids(&core, &alice, "id = -1").is_empty());
    assert!(ids(&core, &alice, "id < -1").is_empty());
}

#[test]
fn malformed_negative_forms_are_rejected_with_42601() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    for bad in [
        "qty = - -1",
        "qty = -qty",
        "qty = -(1)",
        "qty = -'1'",
        "qty IN (-1, 'a')",
        "qty BETWEEN - AND 1",
    ] {
        assert_eq!(code(&core, &alice, bad), "42601", "{bad}");
    }
}

#[test]
fn numeric_bare_literal_in_unsupported_shapes_stays_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    assert_eq!(code(&core, &alice, "price = 1 + 1"), "22000");
}

#[test]
fn numeric_literal_compare_is_tenant_isolated_with_index() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    let cold = ids(&core, &alice, "price >= -1 AND price <= 2.5");
    for ddl in [
        "CREATE INDEX idx_price ON docs (price)",
        "CREATE INDEX idx_qty ON docs (qty)",
    ] {
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(&alice, &mut session, ddl)
            .unwrap_or_else(|e| panic!("{ddl:?} should succeed, got {e:?}"));
    }
    for w in [
        "price >= -1 AND price <= 2.5",
        "price = 1",
        "qty >= -1",
        "qty = -1",
    ] {
        let hot = ids(&core, &alice, w);
        assert!(!hot.contains(&9), "{w}");
        assert_eq!(hot, ids(&core, &alice, w), "{w}");
    }
    assert_eq!(cold, ids(&core, &alice, "price >= -1 AND price <= 2.5"));
    assert_eq!(ids(&core, &bob, "price = 1"), vec![9]);
}

fn dml_result(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Result<(), String> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
        .map(|_| ())
        .map_err(|e| e.wire_code().to_string())
}

/// 述語形 DML（`USING OPERATION_ID`。content hash 計算経路）は、裸の数値リテラル形でも
/// 文字列リテラル形と同じ成否・同じ `wire_code` になる（2^53 超の整数を含む。RECOVER-10）。
#[test]
fn predicate_dml_with_bare_numeric_literal_matches_string_form() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    for (i, lit) in ["1", "-1", "1e1", "100000000000000000000", "0.5"]
        .into_iter()
        .enumerate()
    {
        let bare = dml_result(
            &core,
            &alice,
            &format!(
                "DELETE FROM {TABLE} WHERE price > {lit} AND id = 999 USING OPERATION_ID 'b-{i}'"
            ),
        );
        let quoted = dml_result(
            &core,
            &alice,
            &format!(
                "DELETE FROM {TABLE} WHERE price > '{lit}' AND id = 999 USING OPERATION_ID 'q-{i}'"
            ),
        );
        assert_eq!(bare, quoted, "{lit}");
    }
    // 実際に作用する DELETE と、同一 operation_id の再送。
    dml_result(
        &core,
        &alice,
        "DELETE FROM docs WHERE price = -3 USING OPERATION_ID 'del-neg'",
    )
    .expect("delete by negative bare literal");
    assert!(ids(&core, &alice, "price = -3").is_empty());
    assert_eq!(
        dml_result(
            &core,
            &alice,
            "DELETE FROM docs WHERE price = -3 USING OPERATION_ID 'del-neg'"
        )
        .unwrap_err(),
        "23505"
    );
    assert_eq!(
        dml_result(
            &core,
            &alice,
            "DELETE FROM docs WHERE price = -1 USING OPERATION_ID 'del-neg'"
        )
        .unwrap_err(),
        "22023"
    );
}
