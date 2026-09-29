//! 層 A 結合テスト（Issue #945・NOSQL-14）: NoSQL `filter` の語彙拡張
//! （範囲比較・`IN`・`OR`）が、`search`（vector）・`scan`・`aggregate` の
//! 3 op で SQL 表層と同一の束縛結果（`declarative_filter::bind_all`／
//! `sql::parser::bind_where_predicates` 経由）になること、上限・拒否系が
//! 期待どおりの `wire_code` に写像されること、`OR` を経由しても RLS 境界
//! （テナント越境）が破れないことを固定する。
//!
//! `nosql7_filter_mapping.rs`（`eq`／`prefix`・AND のみ）との役割分担:
//! 本ファイルは Issue #945 で新規に受理する語彙（範囲比較 6 語彙・`in`・
//! `or`）にのみ焦点を当てる。既存語彙の回帰は `nosql7_filter_mapping.rs`
//! が引き続き担う。
//!
//! ポインタ: `docs/spec/05-tasks.md` TASK-147・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-14・
//! `docs/spec/04-behavior/sql-surface.md` SQL-24。

#[path = "http_common/mod.rs"]
mod http_common;
use http_common::temp_db;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::error_format::ClassifiedError;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::allowlist::{validate_sql, SqlSurfaceError, Statement, TableLookup};
use engine::sql::mode::SessionState;
use engine::sql::parser::{bind, bind_scan, BoundScan};
use engine::sql::udf_call::UdfRegistry;
use engine::storage::{Storage, Visibility};
use wire_server::http::query::filter::{bind_filter, FilterError};
use wire_server::http::query::schema::{schema_for, Validated};

const TABLE: &str = "docs";

fn udfs() -> UdfRegistry {
    UdfRegistry::default()
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("created", ColumnType::Date, true),
            ColumnDef::new(
                "amount",
                ColumnType::Numeric {
                    precision: 5,
                    scale: 2,
                },
                true,
            ),
        ],
    )
}

struct FixedTableLookup;

impl TableLookup for FixedTableLookup {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(name == TABLE)
    }
}

fn sql_side_metadata_filters(
    where_clause: &str,
) -> Vec<engine::declarative_filter::MetadataFilter> {
    let sql = format!(
        "SELECT id, lang FROM {TABLE} WHERE {where_clause} \
         ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10"
    );
    let validated = validate_sql(&sql, &FixedTableLookup).expect("validate_sql");
    let Statement::Select(validated_select) = validated else {
        panic!("expected Statement::Select");
    };
    let bound = bind(&validated_select, &schema()).expect("bind");
    bound.metadata_filters().to_vec()
}

fn sql_side_or_group_count(where_clause: &str) -> usize {
    let sql = format!(
        "SELECT id, lang FROM {TABLE} WHERE {where_clause} \
         ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10"
    );
    let validated = validate_sql(&sql, &FixedTableLookup).expect("validate_sql");
    let Statement::Select(validated_select) = validated else {
        panic!("expected Statement::Select");
    };
    let bound = bind(&validated_select, &schema()).expect("bind");
    bound.or_filters().len()
}

fn filter_items_for_op<'a>(op: &str, value: &'a JsonValue) -> &'a [JsonValue] {
    let schema = schema_for(op).unwrap_or_else(|| panic!("schema_for must resolve {op}"));
    let validated: Validated<'a> = schema.validate(value).expect("op JSON must validate");
    validated
        .optional_array("filter")
        .expect("filter field must be array-typed")
        .expect("filter field must be present in fixture")
}

#[test]
fn range_operators_match_sql_where_across_search_scan_aggregate() {
    let filter_json = r#""filter":[{"column":"created","op":"gt","value":"2024-01-01"},{"column":"amount","op":"lte","value":9.5}]"#;
    let cases = [
        (
            "search",
            format!(
                r#"{{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10,{filter_json}}}"#
            ),
        ),
        (
            "scan",
            format!(r#"{{"op":"scan","table":"docs","limit":500,{filter_json}}}"#),
        ),
        (
            "aggregate",
            format!(
                r#"{{"op":"aggregate","table":"docs","aggregates":[{{"fn":"count","column":"id"}}],{filter_json}}}"#
            ),
        ),
    ];

    let mut results = Vec::new();
    for (op, text) in &cases {
        let value = parse_json(text).expect("valid JSON fixture");
        let items = filter_items_for_op(op, &value);
        let bound = bind_filter(items, &schema(), &udfs()).expect("bind_filter ok");
        results.push(bound.metadata_filters().to_vec());
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(results[1], results[2]);

    // `amount <= 9.5`（裸の数値リテラル）は SQL 表層では式レーン
    // （`udf_call::bind_expr`）へ解釈され、`NUMERIC` 列は式内参照非対応のため
    // 拒否される（`declarative_filter::DeclarativeFilter::
    // compare_numeric_literal` の裸数値リテラル形は Rust API 直接呼び出し
    // 限定——モジュール doc 参照）。SQL 表層で同じ宣言的比較を得るには
    // 引用符付きの数値文字列形（`amount <= '9.5'`）を使う。
    let via_sql = sql_side_metadata_filters("created > '2024-01-01' AND amount <= '9.5'");
    assert_eq!(results[0], via_sql);
}

#[test]
fn le_lte_and_ge_gte_are_accepted_synonyms() {
    for (short, long) in [("le", "lte"), ("ge", "gte")] {
        let short_items = parse_json(&format!(
            r#"[{{"column":"amount","op":"{short}","value":1}}]"#
        ))
        .expect("valid JSON");
        let JsonValue::Array(short_items) = short_items else {
            panic!("expected array");
        };
        let long_items = parse_json(&format!(
            r#"[{{"column":"amount","op":"{long}","value":1}}]"#
        ))
        .expect("valid JSON");
        let JsonValue::Array(long_items) = long_items else {
            panic!("expected array");
        };
        let short_bound = bind_filter(&short_items, &schema(), &udfs()).expect("bind ok");
        let long_bound = bind_filter(&long_items, &schema(), &udfs()).expect("bind ok");
        assert_eq!(
            short_bound.metadata_filters(),
            long_bound.metadata_filters()
        );
    }
}

#[test]
fn in_matches_sql_in_list_across_search_scan_aggregate() {
    let filter_json = r#""filter":[{"column":"lang","op":"in","value":["ja","en","fr"]}]"#;
    let cases = [
        (
            "search",
            format!(
                r#"{{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10,{filter_json}}}"#
            ),
        ),
        (
            "scan",
            format!(r#"{{"op":"scan","table":"docs","limit":500,{filter_json}}}"#),
        ),
        (
            "aggregate",
            format!(
                r#"{{"op":"aggregate","table":"docs","aggregates":[{{"fn":"count","column":"id"}}],{filter_json}}}"#
            ),
        ),
    ];

    let mut results = Vec::new();
    for (op, text) in &cases {
        let value = parse_json(text).expect("valid JSON fixture");
        let items = filter_items_for_op(op, &value);
        let bound = bind_filter(items, &schema(), &udfs()).expect("bind_filter ok");
        results.push(bound.metadata_filters().to_vec());
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(results[1], results[2]);

    let via_sql = sql_side_metadata_filters("lang IN ('ja','en','fr')");
    assert_eq!(results[0], via_sql);
}

#[test]
fn or_group_matches_sql_or_across_search_scan_aggregate() {
    let filter_json = r#""filter":[{"or":[{"column":"lang","op":"eq","value":"ja"},{"column":"created","op":"gt","value":"2024-01-01"}]}]"#;
    let cases = [
        (
            "search",
            format!(
                r#"{{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":10,{filter_json}}}"#
            ),
        ),
        (
            "scan",
            format!(r#"{{"op":"scan","table":"docs","limit":500,{filter_json}}}"#),
        ),
        (
            "aggregate",
            format!(
                r#"{{"op":"aggregate","table":"docs","aggregates":[{{"fn":"count","column":"id"}}],{filter_json}}}"#
            ),
        ),
    ];

    for (op, text) in &cases {
        let value = parse_json(text).expect("valid JSON fixture");
        let items = filter_items_for_op(op, &value);
        let bound = bind_filter(items, &schema(), &udfs()).expect("bind_filter ok");
        assert!(bound.metadata_filters().is_empty(), "op={op}");
        assert_eq!(bound.or_filters().len(), 1, "op={op}");
    }

    let sql_or_count = sql_side_or_group_count("(lang = 'ja' OR created > '2024-01-01')");
    assert_eq!(sql_or_count, 1);
}

#[test]
fn single_branch_or_flattens_to_and_and_matches_sql() {
    let items =
        parse_json(r#"[{"or":[{"column":"lang","op":"eq","value":"ja"}]}]"#).expect("valid JSON");
    let JsonValue::Array(items) = items else {
        panic!("expected array");
    };
    let bound = bind_filter(&items, &schema(), &udfs()).expect("bind ok");
    assert!(bound.or_filters().is_empty());
    let via_sql = sql_side_metadata_filters("lang = 'ja'");
    assert_eq!(bound.metadata_filters(), via_sql.as_slice());
}

#[test]
fn rls_predicate_inside_or_branch_is_rejected() {
    let items = parse_json(
        r#"[{"or":[{"column":"visible","op":"eq","value":"x"},{"column":"lang","op":"eq","value":"ja"}]}]"#,
    )
    .expect("valid JSON");
    let JsonValue::Array(items) = items else {
        panic!("expected array");
    };
    let err = bind_filter(&items, &schema(), &udfs()).expect_err("must reject");
    assert!(matches!(err, FilterError::RlsPredicateNotAllowed));
    assert_eq!(ClassifiedError::wire_code(&err), "42601");
}

#[test]
fn in_empty_and_over_limit_are_rejected_with_expected_wire_codes() {
    let empty = parse_json(r#"[{"column":"lang","op":"in","value":[]}]"#).expect("valid JSON");
    let JsonValue::Array(empty) = empty else {
        panic!("expected array");
    };
    let err = bind_filter(&empty, &schema(), &udfs()).expect_err("must reject");
    assert_eq!(ClassifiedError::wire_code(&err), "42601");

    let mut values = String::from("[");
    for i in 0..=engine::declarative_filter::MAX_IN_LIST_ITEMS {
        if i > 0 {
            values.push(',');
        }
        values.push_str(&format!("\"v{i}\""));
    }
    values.push(']');
    let over_limit = parse_json(&format!(
        r#"[{{"column":"lang","op":"in","value":{values}}}]"#
    ))
    .expect("valid JSON");
    let JsonValue::Array(over_limit) = over_limit else {
        panic!("expected array");
    };
    let err = bind_filter(&over_limit, &schema(), &udfs()).expect_err("must reject");
    assert_eq!(ClassifiedError::wire_code(&err), "54000");
}

#[test]
fn range_on_text_column_binds_to_expression_lane_matching_sql() {
    // Issue #1183: TEXT の範囲比較は式レーンで束縛され、SQL の `lang > 'a'` と
    // 同じ形（式述語 1 件・メタデータフィルタ無し）になる。
    let items = parse_json(r#"[{"column":"lang","op":"gt","value":"a"}]"#).expect("valid JSON");
    let JsonValue::Array(items) = items else {
        panic!("expected array");
    };
    let bound = bind_filter(&items, &schema(), &udfs()).expect("must bind");
    assert!(bound.metadata_filters().is_empty());
    assert_eq!(bound.expr_filters().len(), 1);
}

#[test]
fn eq_and_range_on_integer_column_bind_to_expression_lane() {
    let schema_with_int = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("count", ColumnType::Integer, true),
        ],
    );
    for op in ["eq", "gt"] {
        let items = parse_json(&format!(r#"[{{"column":"count","op":"{op}","value":1}}]"#))
            .expect("valid JSON");
        let JsonValue::Array(items) = items else {
            panic!("expected array");
        };
        let bound = bind_filter(&items, &schema_with_int, &udfs()).expect("must bind");
        assert_eq!(bound.expr_filters().len(), 1, "{op}");
    }
}

type NumericRow = (
    u64,
    &'static str,
    Option<i32>,
    Option<i64>,
    Option<f32>,
    Option<f64>,
);

/// Issue #1183・NOSQL-14・NOSQL-17 ポインタ: 数値列・TEXT 範囲の `filter` が、
/// 同じ条件の SQL `WHERE` と完全に同じ結果集合を返すこと（パリティ）、および
/// 他テナントの行が結果に一切現れないこと（RLS 境界）を、実データの scan 実行で
/// 固定する。
#[test]
fn numeric_and_text_range_filters_match_sql_and_respect_tenant_boundary() {
    let path = temp_db::unique_db_path("nosql14-numeric-parity");
    let _guard = temp_db::CleanupGuard(path.clone());

    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("total", ColumnType::BigInt, true),
            ColumnDef::new("ratio", ColumnType::Real, true),
            ColumnDef::new("score", ColumnType::Double, true),
        ],
    );
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema).expect("create table");
    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx");

    // id 4 は数値列が NULL。id 100・101 は tenant-b（Private）で、いずれの
    // 条件にも一致しうる値。
    let rows_a: [NumericRow; 5] = [
        (1, "alpha", Some(1), Some(10), Some(0.5), Some(-1.5)),
        (2, "beta", Some(2), Some(20), Some(1.5), Some(0.0)),
        (3, "gamma", Some(3), Some(-30), Some(2.5), Some(2.5)),
        (4, "delta", None, None, None, None),
        (5, "\u{3042}", Some(5), Some(50), Some(5.0), Some(5.0)),
    ];
    let rows_b: [NumericRow; 2] = [
        (100, "alpha", Some(2), Some(20), Some(1.5), Some(0.0)),
        (101, "zzz", Some(3), Some(30), Some(2.5), Some(2.5)),
    ];
    for (ctx, vis, rows) in [
        (&ctx_a, Visibility::Public, rows_a.as_slice()),
        (&ctx_b, Visibility::Private, rows_b.as_slice()),
    ] {
        for &(id, lang, qty, total, ratio, score) in rows {
            let op = engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                .expect("valid operation_id");
            engine::tenant::insert_typed_row(
                &storage,
                TABLE,
                ctx,
                id,
                vis,
                &[
                    Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
                    Value::Text(lang.to_string()),
                    qty.map_or(Value::Null, Value::Integer),
                    total.map_or(Value::Null, Value::BigInt),
                    ratio.map_or(Value::Null, Value::Real),
                    score.map_or(Value::Null, Value::Double),
                ],
                &op,
            )
            .expect("insert row");
        }
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let session = SessionState::default();

    // (NoSQL filter JSON, 等価な SQL WHERE)
    let cases: [(&str, &str); 16] = [
        (r#"[{"column":"qty","op":"eq","value":2}]"#, "qty = 2"),
        (r#"[{"column":"qty","op":"gt","value":1}]"#, "qty > 1"),
        (r#"[{"column":"qty","op":"ge","value":3}]"#, "qty >= 3"),
        (r#"[{"column":"qty","op":"lt","value":3}]"#, "qty < 3"),
        (r#"[{"column":"qty","op":"lte","value":2}]"#, "qty <= 2"),
        (r#"[{"column":"total","op":"lt","value":0}]"#, "total < 0"),
        (
            r#"[{"column":"total","op":"gte","value":20}]"#,
            "total >= 20",
        ),
        (
            r#"[{"column":"ratio","op":"gt","value":1.5}]"#,
            "ratio > 1.5",
        ),
        (
            r#"[{"column":"ratio","op":"eq","value":0.5}]"#,
            "ratio = 0.5",
        ),
        (
            r#"[{"column":"score","op":"lt","value":0.5}]"#,
            "score < 0.5",
        ),
        (r#"[{"column":"score","op":"eq","value":0}]"#, "score = 0"),
        (r#"[{"column":"lang","op":"lt","value":"b"}]"#, "lang < 'b'"),
        (
            r#"[{"column":"lang","op":"ge","value":"beta"}]"#,
            "lang >= 'beta'",
        ),
        (
            r#"[{"column":"lang","op":"gt","value":"a"},{"column":"qty","op":"lt","value":5}]"#,
            "lang > 'a' AND qty < 5",
        ),
        (
            r#"[{"or":[{"column":"qty","op":"eq","value":1},{"column":"lang","op":"gt","value":"g"}]}]"#,
            "qty = 1 OR lang > 'g'",
        ),
        (r#"[{"column":"qty","op":"gt","value":100}]"#, "qty > 100"),
    ];
    let run_nosql = |filter_json: &str| {
        let items = parse_json(filter_json).expect("valid JSON");
        let JsonValue::Array(items) = items else {
            panic!("expected array");
        };
        core.execute_bound_scan_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let bound_filters =
                bind_filter(&items, schema, udfs).map_err(FilterError::into_sql_surface_error)?;
            let (metadata_filters, expr_filters, or_filters) = bound_filters.into_parts();
            Ok(BoundScan::new(
                TABLE.to_string(),
                vec![engine::sql::parser::ProjectedColumn::Id],
                metadata_filters,
                expr_filters,
                100,
            )
            .with_or_filters(or_filters))
        })
        .unwrap_or_else(|e| panic!("nosql filter {filter_json} must execute: {e:?}"))
    };
    let run_sql = |where_clause: &str| {
        core.execute_bound_scan_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let sql_validated = validate_sql(
                &format!("SELECT id FROM docs WHERE {where_clause} LIMIT 100"),
                &FixedTableLookupForRls,
            )?;
            let Statement::Scan(validated_scan) = sql_validated else {
                return Err(SqlSurfaceError::Internal {
                    detail: "expected Statement::Scan".to_string(),
                });
            };
            bind_scan(&validated_scan, schema, udfs)
        })
        .unwrap_or_else(|e| panic!("sql {where_clause} must execute: {e:?}"))
    };
    let sorted_rows = |r: &engine::sql::exec::QueryResult| {
        let mut v: Vec<String> = r
            .rows
            .iter()
            .map(|row| format!("{:?}", row.cells))
            .collect();
        v.sort();
        v
    };
    for (filter_json, where_clause) in cases {
        let nosql = run_nosql(filter_json);
        let sql = run_sql(where_clause);
        assert_eq!(
            sorted_rows(&nosql),
            sorted_rows(&sql),
            "NoSQL filter {filter_json} must match SQL WHERE {where_clause}"
        );
        // 他テナント（id 100・101）は決して現れない（RLS 境界）。
        for row in &nosql.rows {
            let cell = format!("{:?}", row.cells);
            assert!(
                !cell.contains("100") && !cell.contains("101"),
                "tenant-b row leaked for {filter_json}: {cell}"
            );
        }
    }

    // 全件空で一致しているだけの空振りを防ぐ（tenant-a の qty > 1 は id 2・3・5）。
    let result = run_nosql(r#"[{"column":"qty","op":"gt","value":1}]"#);
    assert_eq!(result.rows.len(), 3);
}

/// RLS 境界（Issue #945 の受け入れ条件・security.md P0）: 全テナントの行に
/// 一致する `OR`（`lang = 'ja' OR lang IN ('en','fr')`。tenant-a・tenant-b
/// 双方の行に一致する）を `scan` へ渡しても、自テナントの可視行しか
/// 返らないことを、`declarative_predicate` 経由で束縛した `BoundScan` を
/// `sql::scan::execute_scan` へ実際に通して確認する（`bind_filter` の
/// 出力だけでなく実行結果まで検証する）。
#[test]
fn or_filter_still_enforces_tenant_boundary_on_scan_execution() {
    let path = temp_db::unique_db_path("nosql14-or-rls");
    let _guard = temp_db::CleanupGuard(path.clone());

    let schema = TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    );

    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema).expect("create table");
    let ctx_a = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
        .expect("valid tenant-a ctx");
    // tenant-b の行は `Visibility::Private` にする（`Public` だと全テナントに
    // 見える設計のため、`Public` 同士では境界の検証にならない。
    // `crates/engine/tests/sql_scan_public_api.rs::seed_two_tenants` と同じ
    // フィクスチャ方針）。
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant-b ctx");
    let op_a = engine::recovery::required_op_id::OperationId::parse("tenant-a-op-1")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_a,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
            Value::Text("ja".to_string()),
        ],
        &op_a,
    )
    .expect("insert tenant-a row");
    let op_b = engine::recovery::required_op_id::OperationId::parse("tenant-b-op-1")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_b,
        2,
        Visibility::Private,
        &[
            Value::Vector(vec![0.0, 1.0, 0.0, 0.0]),
            Value::Text("en".to_string()),
        ],
        &op_b,
    )
    .expect("insert tenant-b row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let session = SessionState::default();

    let items = parse_json(r#"[{"or":[{"column":"lang","op":"eq","value":"ja"},{"column":"lang","op":"in","value":["en","fr"]}]}]"#)
        .expect("valid JSON");
    let JsonValue::Array(items) = items else {
        panic!("expected array");
    };

    let result = core
        .execute_bound_scan_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let bound_filters =
                bind_filter(&items, schema, udfs).map_err(FilterError::into_sql_surface_error)?;
            let (metadata_filters, expr_filters, or_filters) = bound_filters.into_parts();
            Ok(BoundScan::new(
                TABLE.to_string(),
                vec![engine::sql::parser::ProjectedColumn::Id],
                metadata_filters,
                expr_filters,
                10,
            )
            .with_or_filters(or_filters))
        })
        .expect("execute_bound_scan_in_session");
    assert_eq!(
        result.rows.len(),
        1,
        "tenant-a must see only its own row despite an OR condition matching every tenant"
    );

    // `bind_scan` 経由（SQL テキスト）でも同じ行数になることを確認する
    // （第 2 の実行器を作らない契約の再確認）。
    let sql_result = core
        .execute_bound_scan_in_session(&ctx_a, &session, TABLE, |schema, udfs| {
            let sql_validated = validate_sql(
                "SELECT id FROM docs WHERE lang = 'ja' OR lang IN ('en','fr') LIMIT 10",
                &FixedTableLookupForRls,
            )?;
            let Statement::Scan(validated_scan) = sql_validated else {
                return Err(SqlSurfaceError::Internal {
                    detail: "expected Statement::Scan".to_string(),
                });
            };
            bind_scan(&validated_scan, schema, udfs)
        })
        .expect("execute_bound_scan_in_session (sql)");
    assert_eq!(sql_result.rows.len(), result.rows.len());
}

struct FixedTableLookupForRls;

impl TableLookup for FixedTableLookupForRls {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(name == TABLE)
    }
}
