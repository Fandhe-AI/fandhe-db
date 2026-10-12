//! `FROM` なし定数式 `SELECT`（SQL-43。`docs/spec/04-behavior/` のポインタのみ。
//! Issue #1531）の結合テスト。`tests/sql41_bool_literal_where.rs` と同じ流儀
//! （実 `Storage`＋`CpuScalarProvider`）で、受理形の結果型・列名、列参照／集計／
//! 余剰句の `42601` 拒否、Describe 整合、トランザクション内実行、テナント非依存を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn core_with_table(label: &str, with_table: bool) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    if with_table {
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("lang", ColumnType::Text, false),
                ],
            ))
            .expect("create table");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

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

fn wire_code_of(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    match core.execute_sql_in_session(ctx, &mut SessionState::default(), sql) {
        Ok(o) => panic!("{sql:?} should be rejected, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn single_cell(r: &QueryResult) -> &Cell {
    assert_eq!(r.rows.len(), 1, "exactly one row");
    r.rows[0].cells.first().expect("one cell")
}

fn col(r: &QueryResult, i: usize) -> (&str, Option<&ColumnType>) {
    match &r.columns[i] {
        ColumnMeta::Computed { name, ty } => (name.as_str(), ty.as_ref()),
        other => panic!("expected Computed column, got {other:?}"),
    }
}

#[test]
fn integer_literal_returns_one_row_integer() {
    let (core, path) = core_with_table("sql43-int", false);
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let r = run(&core, &ctx, "SELECT 1");
    assert_eq!(r.columns.len(), 1);
    assert_eq!(col(&r, 0), ("?column?", Some(&ColumnType::Integer)));
    assert_eq!(single_cell(&r), &Cell::SignedInteger(1));
    // 末尾セミコロン・大文字小文字混在・負数。
    assert_eq!(
        single_cell(&run(&core, &ctx, "select 1;")),
        &Cell::SignedInteger(1)
    );
    let neg = run(&core, &ctx, "SeLeCt -7");
    assert_eq!(single_cell(&neg), &Cell::SignedInteger(-7));
}

#[test]
fn literal_types_text_bigint_numeric() {
    let (core, path) = core_with_table("sql43-types", false);
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");

    let t = run(&core, &ctx, "SELECT 'abc'");
    assert_eq!(col(&t, 0).1, Some(&ColumnType::Text));
    assert_eq!(single_cell(&t), &Cell::Text("abc".to_string()));

    let b = run(&core, &ctx, "SELECT 3000000000");
    assert_eq!(col(&b, 0).1, Some(&ColumnType::BigInt));
    assert_eq!(single_cell(&b), &Cell::SignedInteger(3_000_000_000));

    let n = run(&core, &ctx, "SELECT 1.5");
    assert!(matches!(
        col(&n, 0).1,
        Some(ColumnType::Numeric { scale: 1, .. })
    ));
    match single_cell(&n) {
        Cell::Numeric(d) => assert_eq!(d.to_string(), "1.5"),
        other => panic!("expected Numeric, got {other:?}"),
    }

    let big = run(&core, &ctx, "SELECT 99999999999999999999");
    assert!(matches!(col(&big, 0).1, Some(ColumnType::Numeric { .. })));
    match single_cell(&big) {
        Cell::Numeric(d) => assert_eq!(d.to_string(), "99999999999999999999"),
        other => panic!("expected Numeric, got {other:?}"),
    }
}

#[test]
fn multiple_items_alias_and_default_names() {
    let (core, path) = core_with_table("sql43-multi", false);
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let r = run(&core, &ctx, "SELECT 1 AS one, 'x' AS s, 2.5 AS d");
    assert_eq!(r.columns.len(), 3);
    assert_eq!(col(&r, 0).0, "one");
    assert_eq!(col(&r, 1).0, "s");
    assert_eq!(col(&r, 2).0, "d");
    assert_eq!(r.rows[0].cells.len(), 3);

    let f = run(&core, &ctx, "SELECT lower('ABC')");
    assert_eq!(col(&f, 0).0, "lower");
    assert_eq!(single_cell(&f), &Cell::Text("abc".to_string()));
}

#[test]
fn arithmetic_and_date_expressions_evaluate() {
    let (core, path) = core_with_table("sql43-expr", false);
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let r = run(&core, &ctx, "SELECT 1+1");
    // Issue #1539・SQL-42: 整数リテラル同士の算術は PostgreSQL と同じ int4。
    assert_eq!(col(&r, 0).1, Some(&ColumnType::Integer));
    assert_eq!(single_cell(&r), &Cell::SignedInteger(2));
    let d = run(&core, &ctx, "SELECT DATE '2024-01-02'");
    assert_eq!(col(&d, 0).1, Some(&ColumnType::Date));
    assert!(matches!(single_cell(&d), Cell::Date(_)));
    // 同一入力は決定的。
    assert_eq!(
        run(&core, &ctx, "SELECT 1+1"),
        run(&core, &ctx, "SELECT 1+1")
    );
}

#[test]
fn integer_arithmetic_follows_postgres_types_and_errors() {
    let (core, path) = core_with_table("sql43-intarith", false);
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    // 整数除算は 0 方向への切り捨て（int4）。
    let r = run(&core, &ctx, "SELECT 7/2");
    assert_eq!(col(&r, 0).1, Some(&ColumnType::Integer));
    assert_eq!(single_cell(&r), &Cell::SignedInteger(3));
    let r = run(&core, &ctx, "SELECT (7/2)*2");
    assert_eq!(single_cell(&r), &Cell::SignedInteger(6));
    // int4 を超える結果は bigint ではなく 22003（PostgreSQL と同じ）。
    let e = core
        .execute_sql_in_session(&ctx, &mut SessionState::default(), "SELECT 2147483647+1")
        .expect_err("int4 overflow must be rejected");
    assert_eq!(e.wire_code(), "22003");
    // 小数リテラルが混ざる算術は従来どおり double precision。
    let r = run(&core, &ctx, "SELECT 1+1.5");
    assert_eq!(col(&r, 0).1, Some(&ColumnType::Double));
}

#[test]
fn rejects_column_references_with_uniform_code() {
    let (core, path) = core_with_table("sql43-colref", true);
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    for sql in [
        "SELECT *",
        "SELECT id",
        "SELECT lang",
        "SELECT nonexistent_col",
        "SELECT 1 + lang",
    ] {
        assert_eq!(wire_code_of(&core, &ctx, sql), "42601", "{sql}");
    }
    // 実在列と非実在列で応答（コード・メッセージ）が同一。
    let msg = |sql: &str| {
        core.execute_sql_in_session(&ctx, &mut SessionState::default(), sql)
            .expect_err("rejected")
            .to_string()
    };
    assert_eq!(msg("SELECT lang"), msg("SELECT nonexistent_col"));
    assert!(!msg("SELECT lang").contains("lang"));
}

#[test]
fn rejects_aggregates_and_trailing_clauses() {
    let (core, path) = core_with_table("sql43-reject", true);
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    for sql in [
        "SELECT count(*)",
        "SELECT sum(1)",
        "SELECT lower(count(1))",
        "SELECT",
        "SELECT 1,",
        "SELECT 1 WHERE true",
        "SELECT 1 LIMIT 1",
        "SELECT 1 FROM",
        "SELECT NULL",
        "SELECT $1",
        "SELECT 1 UNION SELECT 2",
        "SELECT 1; SELECT 2",
        "EXPLAIN SELECT 1",
        "DECLARE c CURSOR FOR SELECT 1",
        "COPY (SELECT 1) TO STDOUT",
        "CREATE VIEW v AS SELECT 1",
    ] {
        let code = wire_code_of(&core, &ctx, sql);
        assert!(
            matches!(code, "42601" | "0A000"),
            "{sql:?} should be rejected fail-closed, got {code}"
        );
    }
}

#[test]
fn too_many_items_is_payload_too_large() {
    let (core, path) = core_with_table("sql43-limit", false);
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let list = vec!["1"; 1025].join(", ");
    assert_eq!(
        wire_code_of(&core, &ctx, &format!("SELECT {list}")),
        "54000"
    );
    let ok = vec!["1"; 1024].join(", ");
    assert_eq!(
        run(&core, &ctx, &format!("SELECT {ok}")).columns.len(),
        1024
    );
}

#[test]
fn existing_from_forms_are_unchanged() {
    let (core, path) = core_with_table("sql43-existing", true);
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let r = run(&core, &ctx, "SELECT id FROM docs LIMIT 10");
    assert!(r.rows.is_empty());
    assert_eq!(
        wire_code_of(&core, &ctx, "SELECT id FROM nosuch LIMIT 10"),
        "42P01"
    );
}

#[test]
fn describe_matches_execute_and_does_not_evaluate() {
    let (core, path) = core_with_table("sql43-describe", false);
    let _g = CleanupGuard(path);
    let ctx = ctx_for("tenant-a");
    let sql = "SELECT 1 AS a, 'x' AS b, 1.5 AS c, 2 + 3 AS d, lower('Q') AS e";
    let parsed = core.parse_sql(sql).expect("parse");
    let described = core
        .describe_parsed_in_session(&SessionState::default(), &parsed)
        .expect("describe")
        .expect("columns");
    assert_eq!(described, run(&core, &ctx, sql).columns);

    // 0 除算を含む文でも Describe は評価せず列だけ返す。実行は fail-closed。
    let div = "SELECT 1 / 0";
    let parsed = core.parse_sql(div).expect("parse");
    let cols = core
        .describe_parsed_in_session(&SessionState::default(), &parsed)
        .expect("describe must not evaluate")
        .expect("columns");
    assert_eq!(cols.len(), 1);
    assert!(core
        .execute_sql_in_session(&ctx, &mut SessionState::default(), div)
        .is_err());
}

#[test]
fn works_in_transaction_and_for_any_tenant() {
    let (core, path) = core_with_table("sql43-txn", true);
    let _g = CleanupGuard(path);
    let ctx_a = ctx_for("tenant-a");
    let ctx_b = ctx_for("tenant-b");
    assert_eq!(
        run(&core, &ctx_a, "SELECT 1"),
        run(&core, &ctx_b, "SELECT 1")
    );

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx_a, &mut session, &mut txn, "BEGIN")
        .expect("BEGIN");
    // dirty テーブルを作ってからでも動く。
    core.execute_sql_in_txn(
        &ctx_a,
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'en') USING OPERATION_ID 'op-1'",
    )
    .expect("insert in txn");
    match core
        .execute_sql_in_txn(&ctx_a, &mut session, &mut txn, "SELECT 42")
        .expect("const select in txn")
    {
        SqlOutcome::Query(r) => assert_eq!(single_cell(&r), &Cell::SignedInteger(42)),
        other => panic!("expected Query, got {other:?}"),
    }
    core.execute_sql_in_txn(&ctx_a, &mut session, &mut txn, "COMMIT")
        .expect("COMMIT");
}
