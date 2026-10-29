//! `ln`／`log`／`exp`／`sign` の結合テスト（Issue #1907、対象ビヘイビア: SQL-65 (1)(2)。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md`・`docs/spec/04-behavior/error-format.md`）。
//!
//! 正常値は独立オラクル（Rust 標準の数学関数・既知の定数）と比較し、定義域外は
//! `2201E`／`22012`／`22003` の SQLSTATE で fail-closed に拒否されることを固定する。
//! 可視行 0 件の評価はエラーにならない（SQL-26 の defer-on-error）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("label", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    for (tenant, id, label, vis) in [
        ("tenant-a", 1u64, "x", Visibility::Public),
        // 別テナントの Private 行は tenant-a から不可視（RLS 相当を緩めていない確認）。
        ("tenant-b", 2u64, "y", Visibility::Private),
    ] {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}")).expect("op");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_for(tenant),
            id,
            vis,
            &[
                Value::Vector(vec![0.1, 0.2]),
                Value::Text(label.to_string()),
            ],
            &op_id,
        )
        .expect("insert row");
    }
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn first_cell(core: &EngineCore, sql: &str) -> Cell {
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(&ctx, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    assert_eq!(r.rows.len(), 1, "tenant isolation: {sql}");
    r.rows[0].cells[0].clone()
}

fn code_of(core: &EngineCore, sql: &str) -> String {
    let ctx = ctx_for("tenant-a");
    core.execute_sql(&ctx, sql)
        .expect_err("expected error")
        .wire_code()
        .to_string()
}

fn float_of(core: &EngineCore, expr: &str) -> f64 {
    match first_cell(core, &format!("SELECT {expr} FROM docs LIMIT 10")) {
        Cell::Float(v) => v,
        other => panic!("{expr}: expected float, got {other:?}"),
    }
}

/// SQL-65 (1): 正常値（独立オラクルとの比較）。
#[test]
fn projection_matches_independent_oracle() {
    let path = unique_db_path("sql65-lnlog-ok");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let cases: &[(&str, f64)] = &[
        ("ln(1)", 0.0),
        ("ln(2.718281828459045)", std::f64::consts::E.ln()),
        ("ln(10)", 10f64.ln()),
        ("log(100)", 2.0),
        ("log(1000)", 3.0),
        ("log(2, 8)", 3.0),
        ("log(10, 1000)", 3.0),
        ("log(3, 81)", 4.0),
        ("exp(0)", 1.0),
        ("exp(1)", 1f64.exp()),
        ("sign(0 - 3)", -1.0),
        ("sign(0)", 0.0),
        ("sign(2.5)", 1.0),
    ];
    for (expr, expected) in cases {
        let got = float_of(&core, expr);
        assert!(
            (got - expected).abs() <= 1e-12,
            "{expr}: {got} vs {expected}"
        );
    }
}

/// SQL-65 (1): `WHERE`・`ORDER BY` の位置。
#[test]
fn works_in_where_and_order_by() {
    let path = unique_db_path("sql65-lnlog-pos");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs WHERE ln(exp(1)) = 1 ORDER BY sign(id) LIMIT 10",
        )
        .expect("where/order by");
    assert_eq!(r.rows.len(), 1);
    let r = core
        .execute_sql(&ctx, "SELECT id FROM docs WHERE log(100) = 3 LIMIT 10")
        .expect("where false");
    assert_eq!(r.rows.len(), 0);
}

/// SQL-65 (2): 定義域外は `2201E`、底 1 は `22012`、`exp` のあふれは `22003`。
#[test]
fn domain_errors_use_postgres_sqlstates() {
    let path = unique_db_path("sql65-lnlog-err");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    for e in [
        "ln(0)",
        "ln(0 - 1)",
        "log(0)",
        "log(0 - 10)",
        "log(0, 8)",
        "log(2, 0)",
        "log(0 - 2, 8)",
    ] {
        assert_eq!(
            code_of(&core, &format!("SELECT {e} FROM docs LIMIT 10")),
            "2201E",
            "{e}"
        );
    }
    assert_eq!(
        code_of(&core, "SELECT log(1, 10) FROM docs LIMIT 10"),
        "22012"
    );
    for e in ["exp(1000)", "exp(0 - 1000)"] {
        assert_eq!(
            code_of(&core, &format!("SELECT {e} FROM docs LIMIT 10")),
            "22003",
            "{e}"
        );
    }
}

/// SQL-65 (3): 個数不一致は `42883`、型不一致は `42804`。
#[test]
fn arity_and_type_errors() {
    let path = unique_db_path("sql65-lnlog-arity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    for e in ["log()", "log(1, 2, 3)", "ln(1, 2)", "exp()", "sign(1, 2)"] {
        assert_eq!(
            code_of(&core, &format!("SELECT {e} FROM docs LIMIT 10")),
            "42883",
            "{e}"
        );
    }
    assert_eq!(
        code_of(&core, "SELECT ln(label) FROM docs LIMIT 10"),
        "42804"
    );
}

/// SQL-26: 可視行 0 件では定義域外の式でもエラーにならない（defer-on-error）。
#[test]
fn invisible_rows_do_not_trigger_domain_errors() {
    let path = unique_db_path("sql65-lnlog-defer");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(&ctx, "SELECT ln(0) FROM docs WHERE id = 2 LIMIT 10")
        .expect("no visible rows, no error");
    assert_eq!(r.rows.len(), 0);
}

/// 予約名: 同名の UDF 定義は拒否される。UDF 本体からは呼べる。
#[test]
fn reserved_names_and_udf_bodies() {
    let path = unique_db_path("sql65-lnlog-udf");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    for name in ["ln", "log", "exp", "sign"] {
        let err = core
            .execute_sql_in_session(
                &ctx,
                &mut session,
                &format!("CREATE FUNCTION {name}(x) AS x"),
            )
            .expect_err("reserved");
        assert_eq!(err.wire_code(), "42723", "{name}");
    }
    core.execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION lg(x) AS log(2, x)")
        .expect("define lg");
    let r = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT lg(8) FROM docs LIMIT 10")
        .expect("call lg");
    let engine::sql::SqlOutcome::Query(q) = r else {
        panic!("expected query outcome");
    };
    assert_eq!(q.rows[0].cells[0], Cell::Float(3.0));
}
