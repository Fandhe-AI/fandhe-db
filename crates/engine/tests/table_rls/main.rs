//! 統合テストの束ね先バイナリ `table_rls`（`crates/engine/tests/table_rls/` 配下を 1 つのテストターゲットにする）。
//!
//! 個別実行は `cargo test -p fandhe-db-engine --test table_rls <ファイル名>::` のように
//! モジュール名（元のファイル名）で絞る。共通ヘルパー（`src/test_util/`）は本ファイルで
//! 1 回だけ取り込み、各モジュールからは `use crate::<名前>;` で参照する。

#[path = "../../src/test_util/legacy_table.rs"]
mod legacy_table;

#[path = "../../src/test_util/seed_rows.rs"]
mod seed_rows;

#[path = "../../src/test_util/temp_db.rs"]
mod temp_db;

mod column_type_catalog_regression;
mod column_type_codec_roundtrip;
mod column_type_integer;
mod column_type_sql_rejection_matrix;
mod copy_from;
mod copy_from_options;
mod copy_options_parse;
mod copy_prepared;
mod copy_to_table;
mod insert_multi_row;
mod insert_without_vector_column;
mod multi_dim_tables;
mod multi_relation_binding;
mod multi_relation_rls_snapshot;
mod partitioned_dml_bound_session;
mod partitioned_dml_concurrency;
mod partitioned_dml_sql;
mod prepared_declared_conversion;
mod prepared_declared_param_types;
mod prepared_params;
mod recovery_content_hash;
mod recovery_ledger;
mod recovery_ledger_mode_matrix;
mod recovery_optional_op_id;
mod recovery_required_op_id;
mod recovery_two_path;
mod rls10_gin_index_paths;
mod rls10_plpgsql_paths;
mod rls10_recursive_cte_paths;
mod rls10_relational_paths;
mod rls10_sql_function_paths;
mod rls11_read_your_writes;
mod rls13_policy_ddl;
mod rls13_policy_read_path_coverage;
mod rls13_policy_using;
mod rls13_policy_with_check;
mod rls13_row_security;
mod rls_generalized;
mod rls_implicit;
mod rls_safety_net;
mod rls_security;
mod row_codec;
mod row_id_tenant_scope;
mod table15_hnsw_invalidation;
mod table16_check_constraint;
mod table16_primary_key;
mod table17_foreign_key;
mod table18_buffered_outer;
mod table18_view;
mod table19_drop_alter_column;
mod table23_index_rowid_reference;
mod table23_legacy_fixture_migration;
mod table23_legacy_id_migration;
mod table23_no_implicit_id;
mod table23_rowid_storage;
mod table23_search_rowid;
mod table24_alter_sequence;
mod table24_migrated_id_batch;
mod table24_migrated_id_insert;
mod table24_sequence_ddl;
mod table24_sequence_ledger;
mod table24_sequence_nextval;
mod table27_alter_schema;
mod table27_current_schema;
mod table27_qualified_names;
mod table27_schema_ddl;
mod table27_schema_objects;
mod table27_search_path;
mod table27_tenant_boundary;
mod table36_create_if_not_exists;
mod table41_view_column_aliases;
mod table41_view_or_replace;
mod table42_create_materialized_view;
mod table42_matview_index_drop;
mod table42_refresh_materialized_view;
mod table43_view_check_option;
mod table43_view_insert;
mod table43_view_updatability;
mod table43_view_update_delete;
mod table43_view_write_rls_ledger;
mod table44_temp_table_ddl;
mod table44_temp_table_dml;
mod table44_temp_table_isolation;
mod table45_trigger_ddl;
mod table_generation_bump_coverage;
mod tenant_breach;
mod tenant_isolation;
mod tenant_write_error_exhaustive;
mod unique_constraint;
mod unique_index;
mod visible_cache_oracle;
