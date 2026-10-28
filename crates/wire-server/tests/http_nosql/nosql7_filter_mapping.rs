//! 層 A 結合テスト（TASK-175・NOSQL-7。Issue #761）: 公開 API のみを使い
//! `engine::json::parse_json` → `wire_server::http::query::schema::
//! Validated::optional_array`（`"filter"`）→ `wire_server::http::query::
//! filter::bind_filter` の通しを固定する。
//!
//! `filter.rs` 内 `mod tests`（単体）は `TableSchema` を crate 内で直接
//! 組み立てるが、本ファイルは wire-server クレート外から見える公開関数・
//! 型のみを経由する。さらに SQL 表層（`sql::allowlist::validate_sql` →
//! `sql::parser::bind`）が同じ `WHERE <col> = '<lit>' AND <col> LIKE
//! '<prefix>%'` から得る `MetadataFilter` 列と完全一致することを固定し、
//! `declarative_filter` への「同一の実行器への委譲」（第 2 の実行器を
//! 作らない）契約を検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::error_format::ClassifiedError;
use engine::json::{parse_json, JsonValue};
use engine::sql::allowlist::{validate_sql, SqlSurfaceError, Statement, TableLookup};
use engine::sql::parser::bind;
use engine::sql::udf_call::UdfRegistry;
use wire_server::http::query::filter::{bind_filter, FilterError};
use wire_server::http::query::schema::{schema_for, Validated};

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
        ],
    )
}

/// `validate_sql` が要求する `TableLookup` の固定応答実装（本テストは SQL
/// 側の束縛結果を得るためだけに使い、実行そのものは行わない。
/// `crates/engine/tests/core_bound_plan_entry.rs::FixedTableLookup` と同じ
/// 判断）。
struct FixedTableLookup;

impl TableLookup for FixedTableLookup {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(name == TABLE)
    }
}

/// `filter` 配列を持つ `search`／`scan`／`aggregate` op の最小 JSON クエリ
/// オブジェクトから `Validated::optional_array("filter")` で要素列を得る。
fn filter_items_for_op<'a>(op: &str, value: &'a JsonValue) -> &'a [JsonValue] {
    let schema = schema_for(op).unwrap_or_else(|| panic!("schema_for must resolve {op}"));
    let validated: Validated<'a> = schema.validate(value).expect("op JSON must validate");
    validated
        .optional_array("filter")
        .expect("filter field must be array-typed")
        .expect("filter field must be present in fixture")
}

/// SQL 表層（`WHERE <col> = '<lit>' AND <col> LIKE '<prefix>%'`）が
/// `sql::parser::bind` を通して得る `MetadataFilter` 列。
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

#[test]
fn search_scan_aggregate_share_identical_binding_for_same_filter_array() {
    let filter_json = r#""filter":[{"column":"lang","op":"eq","value":"ja"},{"column":"path","op":"prefix","value":"src/"}]"#;
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

    // 3 op すべてで同一の束縛結果になる（NoSQL 表層内での一貫性）。
    assert_eq!(results[0], results[1]);
    assert_eq!(results[1], results[2]);

    // SQL 表層の `WHERE lang = 'ja' AND path LIKE 'src/%'` と完全一致する
    // （`declarative_filter` への委譲が SQL 表層と同一の実行器であることの
    // 固定）。
    let via_sql = sql_side_metadata_filters("lang = 'ja' AND path LIKE 'src/%'");
    assert_eq!(results[0], via_sql);
}

#[test]
fn negation_and_unknown_operators_are_rejected_as_unsupported_syntax() {
    // Issue #945・NOSQL-14 で `gt`（範囲比較）は受理語彙へ移った。本テストは
    // 「語彙外 `op` はすべて拒否する」契約を、範囲比較・`in`・`or`（グループ
    // キーであり `op` 値ではない）・`between`（Issue #1197 で受理語彙へ移った）以外の代表例で固定する（詳細な語彙網羅は
    // `filter.rs` の単体テスト）。
    for op in ["not", "neq", "BETWEEN", "OR"] {
        let text = format!(
            r#"{{"op":"search","table":"docs","vector":[0.1],"limit":10,"filter":[{{"column":"lang","op":"{op}","value":"ja"}}]}}"#
        );
        let value = parse_json(&text).expect("valid JSON fixture");
        let items = filter_items_for_op("search", &value);
        let err = bind_filter(items, &schema(), &udfs()).expect_err("must reject");
        assert!(matches!(err, FilterError::UnsupportedOperator));
        assert_eq!(ClassifiedError::wire_code(&err), "42601");
    }
}

#[test]
fn over_limit_filter_count_is_rejected_with_payload_too_large_across_ops() {
    let mut items_json = String::from("[");
    for i in 0..=engine::declarative_filter::MAX_METADATA_FILTERS {
        if i > 0 {
            items_json.push(',');
        }
        items_json.push_str(&format!(r#"{{"column":"lang","op":"eq","value":"v{i}"}}"#));
    }
    items_json.push(']');

    for (op, extra) in [
        ("search", r#""vector":[0.1],"limit":10,"#),
        ("scan", r#""limit":500,"#),
        (
            "aggregate",
            r#""aggregates":[{"fn":"count","column":"id"}],"#,
        ),
    ] {
        let text = format!(r#"{{"op":"{op}","table":"docs",{extra}"filter":{items_json}}}"#);
        let value = parse_json(&text).expect("valid JSON fixture");
        let items = filter_items_for_op(op, &value);
        let err = bind_filter(items, &schema(), &udfs()).expect_err("must reject");
        assert_eq!(ClassifiedError::wire_code(&err), "54000");
    }
}
