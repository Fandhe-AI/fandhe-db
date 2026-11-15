//! 集合演算の枝での式射影・定数枝（`FROM` なしの `SELECT <式>`）の束縛
//! （SQL-42。`docs/spec/04-behavior/` のポインタのみ。Issue #3270）の結合テスト。
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql_in_session` 経由で使い、
//! 式射影の枝・定数枝の値と結果列、型の統一（`42804`）、未対応形の fail-closed、
//! テナント境界（不可視行を式が評価しない）、Describe と Execute の一致、
//! 埋め込み文脈での fail-closed を固定する。EXPLAIN・網羅的な結果テストは Issue #3271。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run_opt(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    match run_opt(core, ctx, sql).unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}")) {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query for {sql:?}, got {other:?}"),
    }
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    match run_opt(core, ctx, sql) {
        Ok(o) => panic!("{sql:?} should be rejected, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

/// 先頭列を整数として集め、昇順に並べる（集合演算の出力順は仕様外のため）。
fn sorted_ints(r: &QueryResult) -> Vec<i64> {
    let mut v: Vec<i64> = r
        .rows
        .iter()
        .map(|row| match row.cells.first() {
            Some(Cell::SignedInteger(i)) => *i,
            other => panic!("expected SignedInteger, got {other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn sorted_texts(r: &QueryResult) -> Vec<String> {
    let mut v: Vec<String> = r
        .rows
        .iter()
        .map(|row| match row.cells.first() {
            Some(Cell::Text(s)) => s.clone(),
            other => panic!("expected Text, got {other:?}"),
        })
        .collect();
    v.sort();
    v
}

fn first_col_name(r: &QueryResult) -> String {
    match r.columns.first() {
        Some(
            ColumnMeta::Scalar { name, .. }
            | ColumnMeta::Computed { name, .. }
            | ColumnMeta::AliasedId { name }
            | ColumnMeta::Record { name, .. },
        ) => name.clone(),
        other => panic!("unexpected column meta {other:?}"),
    }
}

/// `t(n INTEGER, body TEXT)`・`b(y BIGINT, s TEXT)`。t1 は t に n=1,2,3、b に y=3,4,5 を持つ。
/// t2 は t に n=9、b に y=77 を持つ（t1 の結果・式の評価に現れてはならない）。
fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path("sql42-set-op-expr");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table t");
    storage
        .create_table(&TableSchema::new(
            "b",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("y", ColumnType::BigInt, false),
                ColumnDef::new("s", ColumnType::Text, false),
            ],
        ))
        .expect("create table b");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    let t1 = ctx_for("t1");
    for (id, n, body) in [(1u64, 1i64, "a"), (2, 2, "b"), (3, 3, "c")] {
        run_opt(
            &core,
            &t1,
            &format!(
                "INSERT INTO t (id, embedding, n, body) VALUES ({id}, '[0.{id},0.1]', {n}, '{body}') \
                 USING OPERATION_ID 'seed-t-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    }
    for (id, y, s) in [(1u64, 3i64, "x"), (2, 4, "y"), (3, 5, "z")] {
        run_opt(
            &core,
            &t1,
            &format!(
                "INSERT INTO b (id, embedding, y, s) VALUES ({id}, '[0.{id},0.1]', {y}, '{s}') \
                 USING OPERATION_ID 'seed-b-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    }
    let t2 = ctx_for("t2");
    run_opt(
        &core,
        &t2,
        "INSERT INTO t (id, embedding, n, body) VALUES (9, '[0.9,0.1]', 9, 'zz') \
         USING OPERATION_ID 'seed-t-other'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    run_opt(
        &core,
        &t2,
        "INSERT INTO b (id, embedding, y, s) VALUES (9, '[0.9,0.1]', 77, 'zz') \
         USING OPERATION_ID 'seed-b-other'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    (core, ctx, CleanupGuard(path))
}

#[test]
fn constant_branch_unions_with_table_branch() {
    let (core, ctx, _g) = setup();
    // 受入例: 左が定数枝。整数リテラル（int4）と BIGINT 列は互換で、結果列は BIGINT へ拡大する。
    let r = run(&core, &ctx, "SELECT 7 UNION SELECT y FROM b");
    assert_eq!(sorted_ints(&r), vec![3, 4, 5, 7]);
    assert_eq!(first_col_name(&r), "?column?");
    match r.columns.first() {
        Some(ColumnMeta::Computed { ty: Some(ty), .. }) => {
            assert_eq!(*ty, ColumnType::BigInt, "result column widens to BIGINT");
        }
        other => panic!("expected a widened Computed column, got {other:?}"),
    }
    // 定数枝が右。
    let r = run(&core, &ctx, "SELECT y FROM b UNION SELECT 7");
    assert_eq!(sorted_ints(&r), vec![3, 4, 5, 7]);
    // 重複除去（UNION）と重複保持（UNION ALL）。
    let r = run(&core, &ctx, "SELECT 3 UNION SELECT y FROM b");
    assert_eq!(sorted_ints(&r), vec![3, 4, 5]);
    let r = run(&core, &ctx, "SELECT 3 UNION ALL SELECT y FROM b");
    assert_eq!(sorted_ints(&r), vec![3, 3, 4, 5]);
    // 末尾 `;` 付き（psql の単独送信と同じ形）。
    let r = run(&core, &ctx, "SELECT 7 UNION SELECT y FROM b;");
    assert_eq!(sorted_ints(&r), vec![3, 4, 5, 7]);
    // 定数式（算術・関数呼び出し・別名）。
    let r = run(&core, &ctx, "SELECT 2*3+1 AS k UNION SELECT y FROM b");
    assert_eq!(sorted_ints(&r), vec![3, 4, 5, 7]);
    assert_eq!(first_col_name(&r), "k");
}

#[test]
fn expression_branches_are_bound_like_plain_selects() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT n+1 FROM t UNION ALL SELECT n FROM t");
    assert_eq!(sorted_ints(&r), vec![1, 2, 2, 3, 3, 4]);
    let r = run(&core, &ctx, "SELECT n+1 FROM t UNION SELECT n FROM t");
    assert_eq!(sorted_ints(&r), vec![1, 2, 3, 4]);
    let r = run(
        &core,
        &ctx,
        "SELECT abs(n) FROM t INTERSECT SELECT n FROM t",
    );
    assert_eq!(sorted_ints(&r), vec![1, 2, 3]);
    // 列名は左端の枝の別名。
    let r = run(&core, &ctx, "SELECT n AS x FROM t EXCEPT SELECT 1");
    assert_eq!(sorted_ints(&r), vec![2, 3]);
    assert_eq!(first_col_name(&r), "x");
    // 右の枝が式でも同じ。
    let r = run(&core, &ctx, "SELECT n FROM t UNION SELECT n*10 FROM t");
    assert_eq!(sorted_ints(&r), vec![1, 2, 3, 10, 20, 30]);
    // 文字列式。
    let r = run(
        &core,
        &ctx,
        "SELECT upper(body) FROM t UNION SELECT s FROM b",
    );
    assert_eq!(sorted_texts(&r), vec!["A", "B", "C", "x", "y", "z"]);
}

#[test]
fn parenthesized_branch_with_expression_and_order_by() {
    let (core, ctx, _g) = setup();
    // 枝内の ORDER BY は基底列キー（式の別名と無関係）。
    let r = run(
        &core,
        &ctx,
        "(SELECT n+1 FROM t ORDER BY n LIMIT 2) UNION SELECT 0",
    );
    assert_eq!(sorted_ints(&r), vec![0, 2, 3]);
    // ORDER BY の別名は式（別名側）を指す。基底列と同名の別名でも別名側が優先される。
    let r = run(
        &core,
        &ctx,
        "(SELECT 10-n AS m FROM t ORDER BY m LIMIT 1) UNION SELECT 0",
    );
    assert_eq!(sorted_ints(&r), vec![0, 7]);
    let r = run(
        &core,
        &ctx,
        "(SELECT 10-n AS m FROM t ORDER BY m DESC LIMIT 1) UNION SELECT 0",
    );
    assert_eq!(sorted_ints(&r), vec![0, 9]);
    let r = run(
        &core,
        &ctx,
        "(SELECT 10-n AS n FROM t ORDER BY n LIMIT 1) UNION SELECT 0",
    );
    assert_eq!(sorted_ints(&r), vec![0, 7]);
}

#[test]
fn trailing_offset_belongs_to_the_whole_operation() {
    let (core, ctx, _g) = setup();
    // 全体 OFFSET を定数枝の OFFSET と取り違えない（取り違えると 7 の行が黙って消える）。
    let r = run(&core, &ctx, "SELECT n FROM t UNION SELECT 7 OFFSET 1");
    assert_eq!(r.rows.len(), 3);
    assert!(sorted_ints(&r).contains(&7));
    let r = run(&core, &ctx, "SELECT n FROM t UNION SELECT 7 LIMIT 2");
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn view_branches_keep_the_view_column_scope() {
    let (core, ctx, _g) = setup();
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    core.execute_sql_in_session(&ctx, &mut ddl, "CREATE VIEW v AS SELECT body FROM t")
        .expect("create view");
    let r = run(
        &core,
        &ctx,
        "SELECT upper(body) FROM v UNION SELECT s FROM b",
    );
    assert_eq!(sorted_texts(&r), vec!["A", "B", "C", "x", "y", "z"]);
    // ビューが公開していない列は式の中でも参照できない。
    assert_eq!(
        code(&core, &ctx, "SELECT n+1 FROM v UNION SELECT y FROM b"),
        "42703"
    );
    // ビュー本文には定数枝も式枝も書けない。
    for sql in [
        "CREATE VIEW w AS SELECT 1 UNION SELECT n FROM t",
        "CREATE VIEW w AS SELECT n+1 FROM t UNION SELECT n FROM t",
    ] {
        let mut ddl = SessionState::default();
        ddl.allow_ddl();
        let err = core
            .execute_sql_in_session(&ctx, &mut ddl, sql)
            .expect_err("must be rejected");
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}

#[test]
fn type_mismatch_is_rejected_with_42804() {
    let (core, ctx, _g) = setup();
    for sql in [
        // 整数リテラルと TEXT 列。
        "SELECT 7 UNION SELECT s FROM b",
        "SELECT s FROM b UNION SELECT 7",
        // 型の決まらない文字列リテラルは他の枝の型へ解決しない（fail-closed）。
        "SELECT 'a' UNION SELECT y FROM b",
        // 列数の不一致。
        "SELECT 1, 2 UNION SELECT y FROM b",
        "SELECT y FROM b UNION SELECT n, n FROM t",
        // 式の結果型と列の型の不一致。
        "SELECT n+1 FROM t UNION SELECT body FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42804", "{sql}");
    }
    // VECTOR 列と整数式は型不一致。
    assert_eq!(
        code(&core, &ctx, "SELECT embedding FROM t UNION SELECT 7"),
        "42804"
    );
}

#[test]
fn unsupported_forms_stay_fail_closed() {
    let (core, ctx, _g) = setup();
    for (sql, expected) in [
        // 全枝が定数の集合演算。
        ("SELECT 1 UNION SELECT 2", "0A000"),
        ("SELECT 1 UNION ALL SELECT 2;", "0A000"),
        // 定数枝の集計・WHERE・HAVING。
        ("SELECT count(*) UNION SELECT y FROM b", "42601"),
        ("SELECT 7 WHERE true UNION SELECT y FROM b", "42601"),
        ("SELECT y FROM b UNION SELECT 7 WHERE true", "42601"),
        // 括弧付きの定数枝の LIMIT（従来どおり FROM を要求する）。
        ("(SELECT 7 LIMIT 1) UNION SELECT y FROM b", "42601"),
        // 枝内の窓関数・スカラーサブクエリ項目・距離列。
        (
            "SELECT row_number() OVER (ORDER BY n) FROM t UNION SELECT n FROM t",
            "42601",
        ),
        (
            "SELECT (SELECT max(y) FROM b) FROM t UNION SELECT n FROM t",
            "42601",
        ),
        // 式の中の集計。
        (
            "SELECT coalesce(sum(n), 0) FROM t UNION SELECT n FROM t",
            "42601",
        ),
        // 定数枝の列参照。
        ("SELECT n UNION SELECT y FROM b", "42703"),
        // NULL リテラルは定数式 SELECT と同じく未対応（引用付き "NULL" は列参照）。
        ("SELECT NULL UNION SELECT y FROM b", "42703"),
        ("SELECT \"NULL\" UNION SELECT y FROM b", "42703"),
        // EXPLAIN は未対応（tracked: #3271）。
        ("EXPLAIN SELECT 7 UNION SELECT y FROM b", "0A000"),
        ("EXPLAIN SELECT n+1 FROM t UNION SELECT n FROM t", "0A000"),
    ] {
        assert_eq!(code(&core, &ctx, sql), expected, "{sql}");
    }
}

#[test]
fn tenant_boundary_holds_for_expression_branches() {
    let (core, ctx, _g) = setup();
    // t2 の行（n = 9）だけが 0 除算を起こす。t1 の式評価は t1 の可視行だけに掛かる。
    let r = run(&core, &ctx, "SELECT 10/(n-9) FROM t UNION SELECT 0");
    assert_eq!(sorted_ints(&r), vec![-1, 0]);
    // 他テナントの行は結果・重複除去・件数のどこにも現れない。
    let r = run(&core, &ctx, "SELECT y+1 FROM b UNION ALL SELECT 7");
    assert_eq!(sorted_ints(&r), vec![4, 5, 6, 7]);
    assert!(!sorted_ints(&r).contains(&78));
    // 逆に t2 から見ても t1 の行は現れない。
    let r = run(&core, &ctx_for("t2"), "SELECT y FROM b UNION SELECT 7");
    assert_eq!(sorted_ints(&r), vec![7, 77]);
}

#[test]
fn describe_matches_execute_columns() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT 7 UNION SELECT y FROM b",
        "SELECT y FROM b UNION SELECT 7",
        "SELECT n+1 AS m FROM t UNION SELECT n FROM t",
        "SELECT n FROM t UNION ALL SELECT 1 UNION SELECT n+2 FROM t",
        "SELECT n AS x FROM t EXCEPT SELECT 1;",
    ] {
        let parsed = core.parse_sql(sql).expect("parse_sql should succeed");
        let described = core
            .describe_parsed_in_session(&SessionState::default(), &parsed)
            .unwrap_or_else(|e| panic!("describe {sql:?} should succeed: {e:?}"))
            .expect("set operation must produce result columns");
        let executed = run(&core, &ctx, sql);
        assert_eq!(described, executed.columns, "{sql}");
        // セッションレス経路とセッション経由の実行は一致する。
        let sessionless = core.execute_sql(&ctx, sql).expect("session-less execute");
        assert_eq!(sessionless.columns, executed.columns, "{sql}");
        assert_eq!(sorted_ints(&sessionless), sorted_ints(&executed), "{sql}");
    }
    // Describe も実行と同じ拒否。
    for (sql, expected) in [
        ("SELECT 7 UNION SELECT s FROM b", "42804"),
        ("SELECT 1 UNION SELECT 2", "0A000"),
    ] {
        let described = core
            .parse_sql(sql)
            .and_then(|p| core.describe_parsed_in_session(&SessionState::default(), &p));
        let code = match described {
            Ok(_) => panic!("{sql:?} must be rejected at describe time"),
            Err(e) => e.wire_code().to_string(),
        };
        assert_eq!(code, expected, "{sql}");
    }
}

/// 埋め込み文脈（CTE・派生表・`IN`・`INSERT ... SELECT`・CTAS・カーソル・再帰 CTE）では、
/// トップレベルと同じ結果になるか、既存の fail-closed コードで拒否される（panic・`XX000` にならない）。
#[test]
fn embedded_contexts_succeed_or_fail_closed() {
    let (core, ctx, _g) = setup();
    for sql in [
        "WITH c AS (SELECT 7 UNION SELECT y FROM b) SELECT * FROM c",
        "WITH c AS (SELECT n FROM t) SELECT n+1 FROM c UNION SELECT 0 FROM c",
        "SELECT * FROM (SELECT 7 UNION SELECT y FROM b) AS d",
        "SELECT n FROM t WHERE n IN (SELECT 7 UNION SELECT y FROM b)",
        "INSERT INTO t (id, embedding, n, body) SELECT 50, '[0.5,0.1]', 7 UNION SELECT y FROM b",
        "CREATE TABLE ctas1 AS SELECT 7 AS v UNION SELECT y FROM b",
        "DECLARE c1 CURSOR FOR SELECT 7 UNION SELECT y FROM b",
        "WITH RECURSIVE r AS (SELECT 1 AS v UNION SELECT v+1 FROM r) SELECT * FROM r",
        "SELECT 7 UNION SELECT y FROM b ORDER BY 1",
    ] {
        let mut session = SessionState::default();
        session.allow_ddl();
        match core.execute_sql_in_session(&ctx, &mut session, sql) {
            Ok(_) => {}
            Err(e) => {
                let c = e.wire_code();
                assert!(
                    matches!(
                        c,
                        "42601" | "0A000" | "42804" | "42P01" | "42703" | "54000" | "25P01"
                    ),
                    "{sql:?} failed with unexpected code {c}: {e:?}"
                );
            }
        }
    }
}

/// 疑似列 `id` は別名の有無にかかわらず集合演算の枝どうしで型互換になる（括弧付き枝も同じ）。
#[test]
fn aliased_and_plain_id_columns_are_compatible() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT id AS x FROM t UNION SELECT id FROM t",
        "SELECT id FROM t UNION SELECT id AS x FROM t",
        "(SELECT id AS x FROM t) UNION (SELECT id FROM t)",
        "(SELECT id FROM t ORDER BY id LIMIT 2) UNION ALL (SELECT id AS x FROM t)",
    ] {
        let r = run(&core, &ctx, sql);
        assert_eq!(r.columns.len(), 1, "{sql}");
    }
}

/// 定数枝のルーチン呼び出しは Describe でも Execute と同じ `0A000` で拒否される。
#[test]
fn routine_call_in_const_branch_is_rejected_in_describe_and_execute() {
    let (core, ctx, _g) = setup();
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE FUNCTION f() RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$",
    )
    .expect("create function");
    let sql = "SELECT f() UNION SELECT n FROM t";
    let exec_code = match core.execute_sql_in_session(&ctx, &mut session, sql) {
        Ok(o) => panic!("{sql:?} should be rejected, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    };
    let described = core
        .parse_sql(sql)
        .and_then(|p| core.describe_parsed_in_session(&session, &p));
    let desc_code = match described {
        Ok(_) => panic!("{sql:?} must be rejected at describe time"),
        Err(e) => e.wire_code().to_string(),
    };
    assert_eq!(exec_code, "0A000");
    assert_eq!(desc_code, exec_code);
}
