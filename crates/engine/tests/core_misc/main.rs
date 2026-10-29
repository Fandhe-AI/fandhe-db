//! 統合テストの束ね先バイナリ `core_misc`（`crates/engine/tests/core_misc/` 配下を 1 つのテストターゲットにする）。
//!
//! 個別実行は `cargo test -p fandhe-db-engine --test core_misc <ファイル名>::` のように
//! モジュール名（元のファイル名）で絞る。共通ヘルパー（`src/test_util/`）は本ファイルで
//! 1 回だけ取り込み、各モジュールからは `use crate::<名前>;` で参照する。

#[path = "../../src/test_util/legacy_table.rs"]
mod legacy_table;

#[path = "../../src/test_util/seed_rows.rs"]
mod seed_rows;

#[path = "../../src/test_util/temp_db.rs"]
mod temp_db;

mod aggregate_scalar_types;
mod ann_future_work_doc;
mod arbitrary_table;
mod arena;
mod array_extended_elem_types;
mod avg_integer_numeric;
mod batch_fallback;
mod batch_limits;
mod bigint_f64_exact_range;
mod boolean_column;
mod bound_plan_public_api;
mod bytea_column;
mod catalog_fill_value_separation;
mod chunking;
mod cjk_tokenizer_impact;
mod commit_boundary;
mod composite_types;
mod concurrent_write;
mod core_api_stability;
mod core_bound_plan_entry;
mod core_explain_bound_entries;
mod core_explain_plan_entry;
mod datetime_column;
mod declarative_filter;
mod decode_tier_scalar_types;
mod default_preset;
mod describe_parity;
mod dictionary;
mod dispatch;
mod enum_column;
mod error_format;
mod error_format_err3;
mod explain_index_names;
mod explain_scalar_plan_declarations;
mod expression_index_planner;
mod extensions;
mod fk_referential_actions;
mod group_by_limits;
mod hnsw;
mod hnsw_cache;
mod hnsw_index_failure_injection;
mod hnsw_provider;
mod hnsw_subset_mask_scan;
mod hybrid;
mod icu_collator_smoke;
mod incremental_index;
mod incremental_index_hnsw;
mod index_declaration_bytea;
mod index_declaration_numeric;
mod index_declaration_targets;
mod index_failure_injection;
mod join_limits;
mod json_acceptance;
mod json_column;
mod json_containment_operators;
mod json_functions;
mod json_jsonpath_operators;
mod json_modify_operators;
mod json_path_functions;
mod json_path_operators;
mod json_text_operators;
mod jsonpath_parse;
mod mode_estimation;
mod nesting_limits;
mod numeric_column;
mod numeric_column_predicates;
mod parallel_search;
mod partial_index_planner;
mod plan_rls_boost;
mod predicate_dml_failure_injection;
mod query_planner;
mod recover12_explicit_txn_resend;
mod regex_match;
mod rerank;
mod rerank_cross_encoder;
mod result_limits;
mod scalar_index_aggregate;
mod scalar_index_bytea;
mod scalar_index_cache;
mod scalar_index_mask_search;
mod scalar_index_numeric;
mod scalar_index_prune;
mod scalar_index_typed_range;
mod scalar_types_float_roundtrip;
mod scalar_types_predicates;
mod scan_limits;
mod schema_limits;
mod scoring_boost;
mod search_engine;
mod similar_to_match;
mod soft_boost;
mod sparse;
mod sparse_cache;
mod sparse_determinism;
mod sparse_stopwords;
mod tiering;
mod transaction_limits;
mod truncate_table;
mod update_row_without_vector_column;
mod uuid_column;
mod vector_core;
mod window_limits;
mod wire16_implicit_txn;
