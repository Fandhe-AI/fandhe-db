//! 結合テストの束ね先 `http_nosql`。各ファイルを `mod` として束ね、テストバイナリ数を減らす
//! （方針・個別実行方法は README「wire-server の結合テストの束ね方と個別実行」）。

#[path = "../common/mod.rs"]
mod common;
#[path = "../http_common/mod.rs"]
mod http_common;
// `http_common` が `temp_db` を取り込んでいるため、二重取り込み（`clippy::duplicate_mod`）を避けて再公開名で参照する。
use http_common::temp_db;
#[path = "../../../engine/src/test_util/legacy_table.rs"]
mod legacy_table;

mod err4_http_projection;
mod http11_limits;
mod http12_fail_closed;
mod http2_framing;
mod http3_content_type;
mod http5_query_bearer;
mod http8_session_close;
mod http_insert_migrated_id;
mod http_limits;
mod nosql10_explain;
mod nosql11_response_schema;
mod nosql12_affected_rows_limit;
mod nosql12_partitioned_dml;
mod nosql12_predicate_dml_composite_filter;
mod nosql12_predicate_dml_numeric_filter;
mod nosql12_update_delete;
mod nosql13_ddl;
mod nosql14_filter_operators;
mod nosql14_query_composite_in_filter;
mod nosql15_aggregate_sort_offset;
mod nosql15_offset;
mod nosql15_scan_sort;
mod nosql16_explain_targets;
mod nosql16_multi_group_by;
mod nosql17_typed_json_parity;
mod nosql1_endpoint_routing;
mod nosql1_op_vocabulary;
mod nosql2_search;
mod nosql2_search_binding;
mod nosql3_scan_mapping;
mod nosql3_scan_wire_parity;
mod nosql4_5_aggregate_wire_parity;
mod nosql4_aggregate;
mod nosql5_group_by;
mod nosql6_insert;
mod nosql6_tenant_row_id_scope;
mod nosql7_filter_mapping;
mod nosql8_schema_validation;
mod nosql9_op_allowlist;
mod nosql_api_doc;
mod nosql_api_doc_examples;
mod nosql_bigint_f64_exact_range;
mod nosql_or_in_rewrite;
mod nosql_result_limits;
mod table23_nosql_pk_row_address;
