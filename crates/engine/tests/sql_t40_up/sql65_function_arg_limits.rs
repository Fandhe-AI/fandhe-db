//! 関数呼び出しの引数上限の結合テスト（Issue #1910、対象ビヘイビア: SQL-65 (3)・SQL-26。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md`）。
//!
//! 1 回の関数呼び出しは 100 個まで受理し、超過は `54023`、未知の関数名・個数不一致は
//! `42883`。`COALESCE` の上限（32・`54000`）は変えないことも固定する。

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

fn concat_sql(n: usize, arg: &str) -> String {
    format!("concat({})", vec![arg; n].join(", "))
}

fn code_of(core: &EngineCore, sql: &str) -> String {
    let ctx = ctx_for("tenant-a");
    core.execute_sql(&ctx, sql)
        .expect_err("expected error")
        .wire_code()
        .to_string()
}

#[test]
fn concat_accepts_up_to_100_arguments() {
    let path = unique_db_path("sql65-concat");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    for n in [4usize, 32, 33, 100] {
        let sql = format!("SELECT id, {} FROM docs LIMIT 10", concat_sql(n, "'a'"));
        let r = core.execute_sql(&ctx, &sql).expect("concat should succeed");
        assert_eq!(r.rows.len(), 1, "tenant isolation n={n}");
        assert_eq!(r.rows[0].cells[1], Cell::Text("a".repeat(n)), "n={n}");
        let sql = format!(
            "SELECT id FROM docs WHERE {} = '{}' LIMIT 10",
            concat_sql(n, "label"),
            "x".repeat(n)
        );
        let r = core.execute_sql(&ctx, &sql).expect("where concat");
        assert_eq!(r.rows.len(), 1, "where n={n}");
    }
}

#[test]
fn over_100_arguments_is_54023_before_function_resolution() {
    let path = unique_db_path("sql65-over");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let sql = format!("SELECT {} FROM docs LIMIT 1", concat_sql(101, "'a'"));
    assert_eq!(code_of(&core, &sql), "54023");
    let sql = format!(
        "SELECT mystery_fn({}) FROM docs LIMIT 1",
        vec!["1"; 101].join(", ")
    );
    assert_eq!(code_of(&core, &sql), "54023");
    assert_eq!(
        code_of(&core, "SELECT mystery_fn(1) FROM docs LIMIT 1"),
        "42883"
    );
}

#[test]
fn fixed_arity_builtins_reject_wrong_counts_with_42883() {
    let path = unique_db_path("sql65-arity");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let lower100 = format!("lower({})", vec!["label"; 100].join(", "));
    for call in [
        "lower(label, label, label, label)",
        "substr(label, 1, 1, 1)",
        "round(1, 2, 3)",
        "date_part('day', now(), 1)",
        lower100.as_str(),
    ] {
        let sql = format!("SELECT {call} FROM docs LIMIT 1");
        assert_eq!(code_of(&core, &sql), "42883", "{call}");
    }
}

#[test]
fn coalesce_keeps_the_32_limit() {
    let path = unique_db_path("sql65-coalesce");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let sql = format!(
        "SELECT coalesce({}) FROM docs LIMIT 1",
        vec!["label"; 33].join(", ")
    );
    assert_eq!(code_of(&core, &sql), "54000");
}

#[test]
fn udf_calls_and_bodies_follow_the_limits() {
    let path = unique_db_path("sql65-udf");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION f1(x) AS x")
        .expect("define f1");
    for (n, code) in [(33usize, "42883"), (100, "42883"), (101, "54023")] {
        let sql = format!(
            "SELECT f1({}) FROM docs LIMIT 1",
            vec!["label"; n].join(", ")
        );
        let err = core
            .execute_sql_in_session(&ctx, &mut session, &sql)
            .expect_err("udf arity");
        assert_eq!(err.wire_code(), code, "n={n}");
    }
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        &format!("CREATE FUNCTION g(x) AS {}", concat_sql(100, "x")),
    )
    .expect("define g with 100-argument concat");
    let r = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT g(label) FROM docs LIMIT 1")
        .expect("call g");
    let engine::sql::SqlOutcome::Query(q) = r else {
        panic!("expected query outcome");
    };
    assert_eq!(q.rows[0].cells[0], Cell::Text("x".repeat(100)));
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!("CREATE FUNCTION h(x) AS {}", concat_sql(101, "x")),
        )
        .expect_err("101 arguments in body");
    assert_eq!(err.wire_code(), "54023");
}

#[test]
fn deeply_nested_concat_arguments_do_not_overflow_the_stack() {
    let path = unique_db_path("sql65-deep");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    // 各引数を入れ子にした 100 引数 concat は、成功するかノード予算超過の 54000 で決定的に拒否される。
    let nested = format!("{}label{}", "lower(".repeat(20), ")".repeat(20));
    let sql = format!("SELECT {} FROM docs LIMIT 1", concat_sql(100, &nested));
    match core.execute_sql(&ctx, &sql) {
        Ok(_) => {}
        Err(e) => assert_eq!(e.wire_code(), "54000"),
    }
}
