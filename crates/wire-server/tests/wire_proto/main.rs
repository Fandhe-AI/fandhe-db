//! 結合テストの束ね先 `wire_proto`。各ファイルを `mod` として束ね、テストバイナリ数を減らす
//! （方針・個別実行方法は README「wire-server の結合テストの束ね方と個別実行」）。

#[path = "../common/mod.rs"]
mod common;
#[path = "../http_common/mod.rs"]
mod http_common;
// `http_common` が `temp_db` を取り込んでいるため、二重取り込み（`clippy::duplicate_mod`）を避けて再公開名で参照する。
use http_common::temp_db;
#[path = "../../../engine/src/test_util/legacy_table.rs"]
mod legacy_table;

mod rls11_read_your_writes;
mod sql69_savepoint_syntax;
mod sql69_txn_aliases;
mod wire11_bind_execute_sync;
mod wire11_message_limits;
mod wire11_parse_describe;
mod wire11_portal_txn_lifetime;
mod wire12_param_binding;
mod wire13_char_typmod;
mod wire13_type_oid;
mod wire14_binary_format;
mod wire14_binary_params;
mod wire14_binary_typed_columns;
mod wire15_cursor;
mod wire15_cursor_portal_binding;
mod wire16_implicit_transaction;
mod wire16_multi_statement;
mod wire1798_limit_omitted_streaming;
mod wire17_copy;
mod wire17_copy_limits;
mod wire19_ready_for_query_status;
mod wire20_copy_atomicity;
mod wire20_copy_multi_statement;
mod wire21_copy_execute;
mod wire21_copy_parse_bind;
mod wire21_copy_sync_recovery;
mod wire22_copy_to_table;
mod wire23_copy_binary_from;
mod wire23_copy_binary_to;
mod wire24_declared_param_oids;
mod wire26_numeric_pg17_vectors;
mod wire_require_operation_id_matrix;
