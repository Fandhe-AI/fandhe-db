//! 結合テストの束ね先 `wire_sql`。各ファイルを `mod` として束ね、テストバイナリ数を減らす
//! （方針・個別実行方法は README「wire-server の結合テストの束ね方と個別実行」）。

#[path = "../common/mod.rs"]
mod common;
#[path = "../http_common/mod.rs"]
mod http_common;
// `http_common` が `temp_db` を取り込んでいるため、二重取り込み（`clippy::duplicate_mod`）を避けて再公開名で参照する。
use http_common::temp_db;
#[path = "../../../engine/src/test_util/legacy_table.rs"]
mod legacy_table;

mod wire_aggregate;
mod wire_array_column;
mod wire_array_multidim_output;
mod wire_bytea_column;
mod wire_create_materialized_view;
mod wire_create_table;
mod wire_create_type;
mod wire_create_view;
mod wire_cte;
mod wire_ddl_add_column;
mod wire_ddl_drop_alter_column;
mod wire_delete_single_row;
mod wire_drop_table;
mod wire_enum_column;
mod wire_error_response;
mod wire_explain;
mod wire_extended_query;
mod wire_float_columns;
mod wire_hint_order;
mod wire_identity_column;
mod wire_index_ddl;
mod wire_insert_operation_id;
mod wire_integer_bigint_column;
mod wire_json_column;
mod wire_lock_table;
mod wire_policy_ddl;
mod wire_result_rows_cap;
mod wire_returning;
mod wire_row_security;
mod wire_scan;
mod wire_scan_offset;
mod wire_scan_order_by;
mod wire_scan_window;
mod wire_schema_ddl;
mod wire_schema_qualified;
mod wire_search_mode;
mod wire_search_path;
mod wire_sequence_ddl;
mod wire_sequence_nextval;
mod wire_sequence_setval;
mod wire_session_timezone;
mod wire_sql25_distinct;
mod wire_sql26_explain_expression_order_having;
mod wire_sql26_extract_order_having;
mod wire_sql29_scalar_subquery;
mod wire_sql42_aggregate_arith_types;
mod wire_sql42_projection_alias;
mod wire_sql42_projection_expr_types;
mod wire_sql43_from_less_select;
mod wire_sql45_param_infer;
mod wire_sql49_upsert_add_param;
mod wire_sql50_update_delete_param;
mod wire_sql53_dml_subquery_operation_id;
mod wire_sql56_order_by_ordinal;
mod wire_sql58_group_by_key_expressions;
mod wire_sql58_group_key_expr_matching;
mod wire_sql58_having_aggregate_calls;
mod wire_sql58_having_without_group_by;
mod wire_sql58_order_by_aggregate_calls;
mod wire_sql67_dollar_quoting;
mod wire_sql78_noneq_between_param;
mod wire_sql78_noneq_between_rls;
mod wire_trigger_ddl;
mod wire_truncate_table;
mod wire_type_name_alias_oid;
mod wire_udf_call;
mod wire_undefined_column;
mod wire_update_single_row;
mod wire_upsert;
mod wire_using_plan;
mod wire_view_check_option;
mod wire_view_insert;
mod wire_view_update_delete;
mod wire_view_write_tenant;
