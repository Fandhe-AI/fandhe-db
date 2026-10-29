//! `left`／`right`／`reverse`／`initcap` の結合テスト（Issue #1902、対象ビヘイビア: SQL-65・
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-65・`docs/spec/05-tasks.md` TASK-308）。
//!
//! `sql26_scalar_functions.rs` と同じ流儀（`unique_db_path`／`CleanupGuard`、実 `Storage`、
//! `EngineCore::execute_sql`／`execute_sql_in_session` を production 経路として検証）で、
//! 投影・WHERE・ORDER BY・GROUP BY・CHECK・予約名・arity・型不一致・RLS を固定する。

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

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("label", ColumnType::Text, true),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// tenant-a に id=1..=4（"hello world"・"日本語テキスト"・NULL・"ABC"）、tenant-b に id=5 を投入する。
fn seed(storage: &Storage) {
    storage.create_table(&schema()).expect("create table");
    let rows: [(&str, u64, Option<&str>); 5] = [
        ("tenant-a", 1, Some("hello world")),
        ("tenant-a", 2, Some("日本語テキスト")),
        ("tenant-a", 3, None),
        ("tenant-a", 4, Some("ABC")),
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
            storage,
            TABLE,
            &ctx_for(tenant),
            id,
            if tenant == "tenant-a" {
                Visibility::Public
            } else {
                Visibility::Private
            },
            &[Value::Vector(vec![0.1, 0.2]), value],
            &op_id,
        )
        .expect("insert row");
    }
}

fn new_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    seed(&storage);
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn cell_for(result: &QueryResult, id: u64, col: usize) -> Cell {
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
fn projection_values_and_null_propagation() {
    let path = unique_db_path("sql65-projection");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id, left(label, 3), right(label, 3), left(label, -2), right(label, -2), \
             reverse(label), initcap(label) FROM docs LIMIT 10",
        )
        .expect("select");
    assert_eq!(cell_for(&r, 1, 1), text("hel"));
    assert_eq!(cell_for(&r, 1, 2), text("rld"));
    assert_eq!(cell_for(&r, 1, 3), text("hello wor"));
    assert_eq!(cell_for(&r, 1, 4), text("llo world"));
    assert_eq!(cell_for(&r, 1, 5), text("dlrow olleh"));
    assert_eq!(cell_for(&r, 1, 6), text("Hello World"));
    assert_eq!(cell_for(&r, 2, 1), text("日本語"));
    assert_eq!(cell_for(&r, 2, 2), text("キスト"));
    assert_eq!(cell_for(&r, 2, 3), text("日本語テキ"));
    assert_eq!(cell_for(&r, 2, 4), text("語テキスト"));
    assert_eq!(cell_for(&r, 2, 5), text("トスキテ語本日"));
    assert_eq!(cell_for(&r, 4, 6), text("Abc"));
    for col in 1..=6 {
        assert_eq!(cell_for(&r, 3, col), Cell::Null);
    }
    // RLS: 他テナントの行は結果に出ない。
    assert!(r.rows.iter().all(|row| row.id != 5));
}

#[test]
fn boundary_lengths() {
    let path = unique_db_path("sql65-boundary");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(
            &ctx,
            "SELECT id, left(label, 0), right(label, 0), left(label, 100), right(label, -100) \
             FROM docs WHERE id = 4",
        )
        .expect("select");
    assert_eq!(cell_for(&r, 4, 1), text(""));
    assert_eq!(cell_for(&r, 4, 2), text(""));
    assert_eq!(cell_for(&r, 4, 3), text("ABC"));
    assert_eq!(cell_for(&r, 4, 4), text(""));
}

#[test]
fn where_order_by_and_group_by_positions() {
    let path = unique_db_path("sql65-positions");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");

    let r = core
        .execute_sql(&ctx, "SELECT id FROM docs WHERE left(label, 1) = 'h'")
        .expect("where left");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].id, 1);

    let r = core
        .execute_sql(&ctx, "SELECT id FROM docs WHERE right(label, 1) = 'C'")
        .expect("where right");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].id, 4);

    let r = core
        .execute_sql(
            &ctx,
            "SELECT id FROM docs WHERE label IS NOT NULL ORDER BY reverse(label) LIMIT 10",
        )
        .expect("order by reverse");
    // reverse: 1="dlrow olleh"、2="トスキテ語本日"、4="CBA" のバイト順昇順。
    let ids: Vec<u64> = r.rows.iter().map(|row| row.id).collect();
    assert_eq!(ids, vec![4, 1, 2]);

    core.execute_sql(
        &ctx,
        "SELECT left(label, 1) AS p, count(*) FROM docs GROUP BY left(label, 1)",
    )
    .expect("group by left");
}

#[test]
fn left_join_keyword_is_not_confused_with_left_function() {
    let path = unique_db_path("sql65-left-join");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    // JOIN の投影位置は関数式を受理しない既存制約がある。`left`／`right` は `substr` と
    // 同じ扱い（同じ結果）になることを固定し、`LEFT JOIN` 自体は壊れていないことを確認する。
    let join = "FROM docs a LEFT JOIN docs b ON a.id = b.id LIMIT 10";
    core.execute_sql(&ctx, &format!("SELECT a.id, b.label {join}"))
        .expect("plain LEFT JOIN");
    let baseline = core
        .execute_sql(&ctx, &format!("SELECT substr(a.label, 1, 1) {join}"))
        .map(|r| r.rows.len())
        .map_err(|e| e.wire_code());
    for f in ["left(a.label, 1)", "right(a.label, 1)"] {
        let got = core
            .execute_sql(&ctx, &format!("SELECT {f} {join}"))
            .map(|r| r.rows.len())
            .map_err(|e| e.wire_code());
        assert_eq!(got, baseline, "{f}");
    }
}

#[test]
fn check_constraint_and_const_expr_insert() {
    let path = unique_db_path("sql65-check");
    let _guard = CleanupGuard(path.clone());
    let ctx = ctx_for("tenant-a");
    {
        let storage = Storage::open(&path).expect("open storage");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            "CREATE TABLE docs (id BIGINT PRIMARY KEY, label TEXT CHECK (left(label, 1) = 'A'))",
        )
        .expect("create table with CHECK");
        core.execute_insert_sql(
            &ctx,
            "INSERT INTO docs (id, label) VALUES (1, initcap('abc def')) USING OPERATION_ID 'op-1'",
        )
        .expect("insert");
        let err = core
            .execute_insert_sql(
                &ctx,
                "INSERT INTO docs (id, label) VALUES (2, 'xyz') USING OPERATION_ID 'op-2'",
            )
            .expect_err("CHECK violation");
        assert_eq!(err.wire_code(), "23514");
        let r = core
            .execute_sql(&ctx, "SELECT id, label FROM docs LIMIT 10")
            .expect("select");
        let row = r.rows.first().expect("one row");
        assert_eq!(row.cells.get(1), Some(&text("Abc Def")));
    }
    let storage = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO docs (id, label) VALUES (3, 'xray') USING OPERATION_ID 'op-3'",
        )
        .expect_err("CHECK violation after reopen");
    assert_eq!(err.wire_code(), "23514");
}

#[test]
fn builtin_names_are_reserved() {
    let path = unique_db_path("sql65-reserved");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    for name in ["left", "right", "reverse", "initcap"] {
        let err = core
            .execute_sql_in_session(
                &ctx,
                &mut session,
                &format!("CREATE FUNCTION {name}(x) AS x"),
            )
            .expect_err(&format!("{name} should be reserved"));
        assert_eq!(err.wire_code(), "42723", "{name}");
    }
}

#[test]
fn arity_type_and_argument_errors() {
    let path = unique_db_path("sql65-errors");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = ctx_for("tenant-a");
    for call in [
        "left(label)",
        "right(label, 1, 2)",
        "reverse()",
        "initcap(label, label)",
    ] {
        let err = core
            .execute_sql(&ctx, &format!("SELECT {call} FROM docs LIMIT 1"))
            .expect_err(call);
        assert_eq!(err.wire_code(), "42883", "{call}");
    }
    for call in ["reverse(vec_norm(embedding))", "left(label, label)"] {
        let err = core
            .execute_sql(&ctx, &format!("SELECT {call} FROM docs LIMIT 1"))
            .expect_err(call);
        assert_eq!(err.wire_code(), "42804", "{call}");
    }
    let err = core
        .execute_sql(&ctx, "SELECT left(label, 1.5) FROM docs LIMIT 1")
        .expect_err("non-integer length");
    assert_eq!(err.wire_code(), "22000");
    // 可視行 0 件では評価されずエラーにならない（SQL-26 の契約）。
    let r = core
        .execute_sql(&ctx, "SELECT left(label, 1.5) FROM docs WHERE id < 0")
        .expect("no visible rows");
    assert!(r.rows.is_empty());
}
