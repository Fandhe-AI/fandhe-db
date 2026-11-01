//! 統合テストの束ね先バイナリ `sql_t01_39`（`crates/engine/tests/sql_t01_39/` 配下を 1 つのテストターゲットにする）。
//!
//! 個別実行は `cargo test -p fandhe-db-engine --test sql_t01_39 <ファイル名>::` のように
//! モジュール名（元のファイル名）で絞る。共通ヘルパー（`src/test_util/`）は本ファイルで
//! 1 回だけ取り込み、各モジュールからは `use crate::<名前>;` で参照する。

#[path = "../../src/test_util/legacy_table.rs"]
mod legacy_table;

#[path = "../../src/test_util/temp_db.rs"]
mod temp_db;

mod sql24_float_large_literal_compare;
mod sql24_in_between_null_not;
mod sql24_like_patterns;
mod sql24_literal_kind_coercion;
mod sql24_numeric_literal_compare;
mod sql24_string_literal_remaining_forms;
mod sql25_aggregate_order_by_scalar_keys;
mod sql25_distinct;
mod sql25_multi_group_by;
mod sql25_offset;
mod sql25_scalar_order_by;
mod sql26_datetime_functions;
mod sql26_extract;
mod sql26_order_by_having_expressions;
mod sql26_scalar_functions;
mod sql27_explain_targets;
mod sql28_inner_join;
mod sql28_multi_way_join;
mod sql28_outer_join;
mod sql29_cte;
mod sql29_projection_subquery;
mod sql29_set_operations;
mod sql29_subquery;
mod sql29_subquery_scalar;
mod sql30_window;
mod sql31_deferred_writer;
mod sql31_deferred_writer_regression;
mod sql31_transaction;
mod sql31_txn_dml;
mod sql31_txn_file_insert_using_plan;
mod sql34_limit_omitted;
mod sql34_result_rows_raised_cap;
mod sql34_subquery_limit_omitted;
mod sql35_recursive_cte_eval;
mod sql35_recursive_cte_search;
mod sql35_recursive_cte_syntax;
mod sql35_recursive_cte_union;
mod sql36_require_where_for_dml;
mod sql36_where_omitted_dml;
mod sql37_current_time_functions;
mod sql37_default_now;
mod sql38_begin_access_mode;
mod sql38_begin_isolation_level;
mod sql38_lock_table;
mod sql38_row_lock_clause;
