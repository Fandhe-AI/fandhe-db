//! `lpad`／`rpad`／`split_part` の結合テスト（Issue #1904、対象ビヘイビア: SQL-65 (1)(2)(3)・
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-65・TASK-308）。
//!
//! `sql26_scalar_functions.rs` と同じ流儀（実 `Storage` ＋ `EngineCore::execute_sql`）で、
//! 投影・WHERE・ORDER BY・CHECK からの利用、NULL 伝播、位置 0 の `22023`、予約名 `42723`、
//! 個数・型不一致、結果長上限 `54000`、テナント境界を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
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
    let rows: [(&str, u64, Option<&str>); 5] = [
        ("tenant-a", 1, Some("a,b,c")),
        ("tenant-a", 2, Some("日本")),
        ("tenant-a", 3, None),
        ("tenant-a", 4, Some("hello")),
        // 別テナントの行は tenant-a から見えてはならない。
        ("tenant-b", 5, Some("secret")),
    ];
    for (tenant, id, label) in rows {
        let value = match label {
            Some(s) => Value::Text(s.to_string()),
            None => Value::Null,
        };
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_for(tenant),
            id,
            if tenant == "tenant-b" {
                Visibility::Private
            } else {
                Visibility::Public
            },
            &[Value::Vector(vec![0.1, 0.2]), value],
            &op_id,
        )
        .expect("insert row");
    }
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn cell(result: &QueryResult, id: u64, col: usize) -> Cell {
    result
        .rows
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("row id={id} not found"))
        .cells
        .get(col)
        .cloned()
        .unwrap_or_else(|| panic!("column {col} missing for id={id}"))
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

#[test]
fn lpad_rpad_project_expected_values_and_propagate_null() {
    let path = unique_db_path("sql65-pad-projection");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id, lpad(label, 7), rpad(label, 7, 'xy'), lpad(label, 3), lpad(label, 0), \
             lpad(label, 6, ''), lpad(label, 6, 'あ') FROM docs LIMIT 10",
        )
        .expect("pad projection");
    assert_eq!(cell(&r, 4, 1), text("  hello"));
    assert_eq!(cell(&r, 4, 2), text("helloxy"));
    assert_eq!(cell(&r, 4, 3), text("hel"));
    assert_eq!(cell(&r, 4, 4), text(""));
    assert_eq!(cell(&r, 4, 5), text("hello"));
    assert_eq!(cell(&r, 4, 6), text("あhello"));
    assert_eq!(cell(&r, 2, 1), text("     日本"));
    assert_eq!(cell(&r, 2, 2), text("日本xyxyx"));
    for col in 1..=6 {
        assert_eq!(cell(&r, 3, col), Cell::Null);
    }
}

#[test]
fn bare_null_literal_arguments_propagate_null() {
    let path = unique_db_path("sql65-bare-null");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    for sql in [
        "SELECT lpad(NULL, 5)",
        "SELECT rpad('a', 5, NULL)",
        "SELECT lpad('a', NULL)",
        "SELECT split_part('a', NULL, 1)",
        "SELECT split_part(NULL, ',', 1)",
        "SELECT split_part('a', ',', NULL)",
    ] {
        match core
            .execute_sql_in_session(&ctx, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        {
            engine::sql::SqlOutcome::Query(r) => {
                assert_eq!(r.rows.len(), 1, "{sql}");
                assert_eq!(r.rows[0].cells, vec![Cell::Null], "{sql}");
            }
            other => panic!("expected a query result for {sql}, got {other:?}"),
        }
    }
    // 非 NULL 引数の型不一致は NULL が混在しても 42804 で拒否される。
    let err = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT lpad(NULL, 'x')")
        .expect_err("type mismatch");
    assert_eq!(err.wire_code(), "42804");
}

#[test]
fn split_part_projects_positive_negative_and_out_of_range() {
    let path = unique_db_path("sql65-split-part");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id, split_part(label, ',', 2), split_part(label, ',', -1), \
             split_part(label, ',', -3), split_part(label, ',', -4), \
             split_part(label, ',', 4), split_part(label, '', 1), split_part(label, '', 2) \
             FROM docs WHERE id = 1 LIMIT 1",
        )
        .expect("split_part projection");
    assert_eq!(cell(&r, 1, 1), text("b"));
    assert_eq!(cell(&r, 1, 2), text("c"));
    assert_eq!(cell(&r, 1, 3), text("a"));
    assert_eq!(cell(&r, 1, 4), text(""));
    assert_eq!(cell(&r, 1, 5), text(""));
    assert_eq!(cell(&r, 1, 6), text("a,b,c"));
    assert_eq!(cell(&r, 1, 7), text(""));

    let r = core
        .execute_sql(
            &ctx,
            "SELECT id, split_part(label, ',', 1) FROM docs WHERE id = 3 LIMIT 1",
        )
        .expect("NULL row");
    assert_eq!(cell(&r, 3, 1), Cell::Null);
}

#[test]
fn functions_work_in_where_and_order_by() {
    let path = unique_db_path("sql65-where-order");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs WHERE split_part(label, ',', 2) = 'b' LIMIT 10",
        )
        .expect("WHERE split_part");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows.first().map(|row| row.id), Some(1));
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs WHERE lpad(label, 7) = '  hello' LIMIT 10",
        )
        .expect("WHERE lpad");
    assert_eq!(r.rows.len(), 1);
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs ORDER BY rpad(label, 8, 'z') LIMIT 10",
        )
        .expect("ORDER BY rpad");
    // tenant-b の行 (id=5) は含まれない。
    assert!(r.rows.iter().all(|row| row.id != 5));
}

#[test]
fn zero_position_is_22023_only_when_a_visible_row_is_evaluated() {
    let path = unique_db_path("sql65-zero-position");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let err = core
        .execute_sql(
            &ctx,
            "SELECT split_part(label, ',', 0) FROM docs WHERE id = 1 LIMIT 1",
        )
        .expect_err("position 0");
    assert_eq!(err.wire_code(), "22023");
    assert!(err.to_string().contains("field position must not be zero"));
    // 可視行が 0 件ならエラーにならない。
    let r = core
        .execute_sql(
            &ctx,
            "SELECT split_part(label, ',', 0) FROM docs WHERE id = 999 LIMIT 1",
        )
        .expect("no visible row");
    assert!(r.rows.is_empty());
}

#[test]
fn names_are_reserved_and_arity_type_errors_are_classified() {
    let path = unique_db_path("sql65-reserved-errors");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    for name in ["lpad", "rpad", "split_part"] {
        let err = core
            .execute_sql_in_session(
                &ctx,
                &mut session,
                &format!("CREATE FUNCTION {name}(x) AS x"),
            )
            .expect_err("reserved");
        assert_eq!(err.wire_code(), "42723", "{name}");
    }
    for sql in [
        "SELECT lpad(label) FROM docs LIMIT 1",
        "SELECT lpad(label, 1, 'a', 'b') FROM docs LIMIT 1",
        "SELECT rpad(label) FROM docs LIMIT 1",
        "SELECT split_part(label, ',') FROM docs LIMIT 1",
    ] {
        let err = core.execute_sql(&ctx, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42883", "{sql}");
    }
    for sql in [
        "SELECT lpad(label, 'x') FROM docs LIMIT 1",
        "SELECT split_part(label, 1, 2) FROM docs LIMIT 1",
    ] {
        let err = core.execute_sql(&ctx, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "42804", "{sql}");
    }
}

#[test]
fn oversized_result_is_rejected_with_54000() {
    let path = unique_db_path("sql65-too-long");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    for sql in [
        "SELECT lpad(label, 5000000) FROM docs WHERE id = 4 LIMIT 1",
        "SELECT rpad(label, 4000000000, 'ab') FROM docs WHERE id = 4 LIMIT 1",
    ] {
        let err = core.execute_sql(&ctx, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "54000", "{sql}");
    }
}

#[test]
fn function_body_may_use_lpad_and_rpad_with_both_arities() {
    let path = unique_db_path("sql65-udf-body");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION f2(x) AS lpad('a', x)")
        .expect("2-arg lpad in body");
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE FUNCTION f3(x) AS rpad('a', x, 'b')",
    )
    .expect("3-arg rpad in body");
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE FUNCTION f4(x) AS split_part('a,b', ',', x)",
    )
    .expect("split_part in body");
    let err = core
        .execute_sql_in_session(&ctx, &mut session, "CREATE FUNCTION f5(x) AS lpad('a')")
        .expect_err("wrong arity in body");
    assert_eq!(err.wire_code(), "42883");
}

#[test]
fn check_constraint_with_lpad_enforces_at_write_time() {
    let path = unique_db_path("sql65-check");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, label TEXT CHECK (split_part(label, ',', 1) = 'ok'))",
    )
    .expect("CREATE TABLE with split_part CHECK");
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO docs (id, label) VALUES (10, 'ok,1') USING OPERATION_ID 'op-chk-ok'",
    )
    .expect("passing row");
    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO docs (id, label) VALUES (11, 'bad,1') USING OPERATION_ID 'op-chk-bad'",
        )
        .expect_err("violating row");
    assert_eq!(err.wire_code(), "23514");
}
