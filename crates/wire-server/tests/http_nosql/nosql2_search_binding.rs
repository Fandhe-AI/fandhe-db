//! 層 A 結合テスト（TASK-175・NOSQL-2。Issue #763）: 公開 API のみを使い
//! `engine::json::parse_json` → `wire_server::http::query::schema::
//! SEARCH_SCHEMA::validate` → `wire_server::http::query::search::bind_search`
//! の通しを固定する。
//!
//! `search.rs` 内 `mod tests`（単体）は各分岐を個別に検証するが、本ファイルは
//! wire-server クレート外から見える公開関数・型のみを経由し、さらに SQL 表層
//! （`sql::allowlist::validate_sql` → `sql::parser::bind`）が同じクエリ意味
//! （`vector`／`ORDER BY <=>`、`filter`／`WHERE`、`columns`／`SELECT` リスト、
//! `mode`／`USING MODE`、`hybrid`／`ORDER BY HYBRID(...)`）から得る
//! `BoundStatement` と**完全一致**することを固定する（NoSQL 表層への
//! `search` 写像が SQL 表層と第 2 の実行器を作らずに委譲している契約の検証）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::json::parse_json;
use engine::sql::allowlist::{validate_sql, SqlSurfaceError, Statement, TableLookup};
use engine::sql::mode::ModeSource;
use engine::sql::parser::{bind, BoundStatement};
use engine::sql::udf_call::UdfRegistry;
use wire_server::http::query::schema::SEARCH_SCHEMA;
use wire_server::http::query::search::{bind_search, BoundSearch};

fn udfs() -> UdfRegistry {
    UdfRegistry::default()
}

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

/// `validate_sql` が要求する `TableLookup` の固定応答実装（本テストは SQL
/// 側の束縛結果を得るためだけに使い、実行そのものは行わない。
/// `crates/wire-server/tests/nosql7_filter_mapping.rs::FixedTableLookup` と
/// 同じ判断）。
struct FixedTableLookup;

impl TableLookup for FixedTableLookup {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(name == TABLE)
    }
}

/// SQL 表層の `sql` を `sql::parser::bind` まで通した `BoundStatement`。
fn sql_side_bound(sql: &str) -> BoundStatement {
    let validated = validate_sql(sql, &FixedTableLookup).expect("validate_sql");
    let Statement::Select(validated_select) = validated else {
        panic!("expected Statement::Select");
    };
    bind(&validated_select, &schema()).expect("bind")
}

/// NoSQL `search` op の JSON クエリオブジェクトを `bind_search` まで通す。
fn nosql_side_bound(text: &str) -> BoundSearch {
    let value = parse_json(text).expect("valid JSON fixture");
    let validated = SEARCH_SCHEMA.validate(&value).expect("must validate shape");
    bind_search(&validated, &schema(), &udfs()).expect("bind_search")
}

fn expect_vector(bound: BoundSearch) -> BoundStatement {
    match bound {
        BoundSearch::Vector(stmt) => stmt,
        BoundSearch::Plan(_) => panic!("expected BoundSearch::Vector"),
    }
}

#[test]
fn c1_vector_only_matches_sql_order_by_distance() {
    let nosql = expect_vector(nosql_side_bound(
        r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10}"#,
    ));
    let sql =
        sql_side_bound("SELECT * FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10");
    assert_eq!(nosql, sql);
}

#[test]
fn c2_vector_with_filter_matches_sql_where_and_order_by() {
    let nosql = expect_vector(nosql_side_bound(
        r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10,"filter":[{"column":"lang","op":"eq","value":"ja"},{"column":"path","op":"prefix","value":"src/"}]}"#,
    ));
    let sql = sql_side_bound(
        "SELECT * FROM docs WHERE lang = 'ja' AND path LIKE 'src/%' \
         ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
    );
    assert_eq!(nosql, sql);
}

#[test]
fn columns_selects_named_projection_matches_sql_select_list() {
    let nosql = expect_vector(nosql_side_bound(
        r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10,"columns":["id","lang"]}"#,
    ));
    let sql = sql_side_bound(
        "SELECT id, lang FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
    );
    assert_eq!(nosql, sql);
}

#[test]
fn mode_recall_matches_sql_using_mode_recall() {
    let nosql = expect_vector(nosql_side_bound(
        r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10,"mode":"recall"}"#,
    ));
    let sql = sql_side_bound(
        "SELECT * FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10 USING MODE 'recall'",
    );
    assert_eq!(nosql, sql);
    assert_eq!(nosql.mode().source(), ModeSource::QueryClause);
}

#[test]
fn mode_precision_matches_sql_using_mode_precision() {
    let nosql = expect_vector(nosql_side_bound(
        r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10,"mode":"precision"}"#,
    ));
    let sql = sql_side_bound(
        "SELECT * FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10 USING MODE 'precision'",
    );
    assert_eq!(nosql, sql);
    assert_eq!(nosql.mode().source(), ModeSource::QueryClause);
}

#[test]
fn mode_absent_matches_sql_without_using_mode_clause() {
    let nosql = expect_vector(nosql_side_bound(
        r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10}"#,
    ));
    let sql =
        sql_side_bound("SELECT * FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10");
    assert_eq!(nosql, sql);
    assert_eq!(nosql.mode().source(), ModeSource::Default);
}

#[test]
fn c4_hybrid_matches_sql_order_by_hybrid_function() {
    let nosql = expect_vector(nosql_side_bound(
        r#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10,"hybrid":{"text":"hello world"}}"#,
    ));
    let sql = sql_side_bound(
        "SELECT * FROM docs \
         ORDER BY HYBRID(embedding, '[0.1,0.2,0.3,0.4]', body, 'hello world') LIMIT 10",
    );
    assert_eq!(nosql, sql);
}

#[test]
fn plan_only_binds_projection_filter_and_limit_matching_a_placeholder_order_by_form() {
    // `plan` 指定は LLM 展開（engine 内部 I/O）を要するため、本ファイルの
    // `bind_search` 単体テストでは `BoundStatement` まで完成させない
    // （実行結線・`BoundStatement` 完成は `EngineCore::
    // execute_bound_plan_search_in_session`。TASK-186・NOSQL-2・
    // Issue #764・`nosql2_search.rs` 参照）。ここでは `PlanSearch` に束縛された
    // 投影・フィルタ・`limit` が、同条件の `ORDER BY` 形 `BoundStatement`
    // （プレースホルダのベクトルリテラルを使った SQL）のアクセサー値と
    // 一致することだけを固定する（`ranking`／`mode` の完成は対象外）。
    let value = parse_json(
        r#"{"op":"search","table":"docs","plan":"find something","limit":10,"filter":[{"column":"lang","op":"eq","value":"ja"}]}"#,
    )
    .expect("valid JSON fixture");
    let validated = SEARCH_SCHEMA.validate(&value).expect("must validate shape");
    let BoundSearch::Plan(plan) = bind_search(&validated, &schema(), &udfs()).expect("bind_search")
    else {
        panic!("expected BoundSearch::Plan");
    };

    let sql = sql_side_bound(
        "SELECT * FROM docs WHERE lang = 'ja' \
         ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
    );

    assert_eq!(plan.table(), sql.table());
    assert_eq!(plan.projection(), sql.projection());
    assert_eq!(plan.metadata_filters(), sql.metadata_filters());
    assert_eq!(plan.limit(), sql.limit());
    assert_eq!(plan.question(), "find something");
}
