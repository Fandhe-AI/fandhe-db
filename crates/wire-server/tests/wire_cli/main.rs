//! 結合テストの束ね先 `wire_cli`。各ファイルを `mod` として束ね、テストバイナリ数を減らす
//! （方針・個別実行方法は README「wire-server の結合テストの束ね方と個別実行」）。

#[path = "../common/mod.rs"]
mod common;
#[path = "../http_common/mod.rs"]
mod http_common;
// `http_common` が `temp_db` を取り込んでいるため、二重取り込み（`clippy::duplicate_mod`）を避けて再公開名で参照する。
use http_common::temp_db;
#[path = "../common/tls_client.rs"]
mod tls_client;

mod backup_cli;
mod batch_max_files_parity;
mod incremental_backup_cli;
mod persist4_second_server_same_db;
mod pitr_restore_cli;
mod recover12_explicit_txn_resend;
mod transaction_limits_parity;
mod verify_restore_cli;
mod wire7_bind_guard;
mod wire_change_log_archive_cli;
mod wire_change_log_cli;
mod wire_correlated_limits_cli;
mod wire_ddl_permission_cli;
mod wire_dml_limits_cli;
mod wire_durability_cli;
mod wire_english_stemmer_cli;
mod wire_function_limits_cli;
mod wire_group_by_limits_cli;
mod wire_hnsw_scope_cli;
mod wire_join_limits_cli;
mod wire_jsonpath_limits_cli;
mod wire_nesting_limits_cli;
mod wire_plpgsql_limits_cli;
mod wire_recursive_cte_limits_cli;
mod wire_regex_limits_cli;
mod wire_require_operation_id_cli;
mod wire_require_where_for_dml_cli;
mod wire_rerank_cli;
mod wire_result_limits_cli;
mod wire_scan_limits_cli;
mod wire_schema_limits_cli;
mod wire_search_engine_cli;
mod wire_search_engine_opt;
mod wire_sparsevec_limits_cli;
mod wire_statement_timeout_cli;
mod wire_temp_table_limits_cli;
mod wire_text_analysis_cli;
mod wire_text_query_limits_cli;
mod wire_tls_cli;
mod wire_transaction_limits_cli;
mod wire_trigger_limits_cli;
mod wire_ts_headline_limits_cli;
mod wire_wasm_limits_cli;
mod wire_window_limits_cli;
