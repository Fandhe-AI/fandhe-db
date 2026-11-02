//! PL/pgSQL の例外条件名（`WHEN unique_violation THEN ...`・`RAISE division_by_zero`）と
//! SQLSTATE の対応、および `EXCEPTION WHEN` の照合規則（Issue #2227・SQL-70）。
//!
//! 条件名の表は PostgreSQL 17 の公開ドキュメント（付録 A「PostgreSQL エラーコード」）の
//! エラー条件名のうち、本 DB が発生させる・利用者が捕捉しうるものを収めた静的表。名前は小文字で
//! 引く（PL/pgSQL は条件名の大文字小文字を区別しない）。1 つの名前が複数コードに対応する場合
//! （`string_data_right_truncation` 等）、`WHEN` は全コードに一致し、`RAISE` は表の先頭を使う。
//! `plpgsql::parser` が定義時に条件名を解決し、`plpgsql::interp` が実行時に [`matches_code`] で
//! 照合する。照合に使う SQLSTATE は、利用者指定（RAISE）ならそのコード、それ以外は
//! `SqlSurfaceError::wire_code()`（`effective_sqlstate`）。

use crate::sql::allowlist::SqlSurfaceError;

/// 条件名と SQLSTATE の表（名前は小文字）。
const CONDITIONS: &[(&str, &str)] = &[
    ("successful_completion", "00000"),
    ("warning", "01000"),
    ("dynamic_result_sets_returned", "0100C"),
    ("implicit_zero_bit_padding", "01008"),
    ("null_value_eliminated_in_set_function", "01003"),
    ("privilege_not_granted", "01007"),
    ("privilege_not_revoked", "01006"),
    ("string_data_right_truncation", "01004"),
    ("string_data_right_truncation", "22001"),
    ("deprecated_feature", "01P01"),
    ("no_data", "02000"),
    ("no_additional_dynamic_result_sets_returned", "02001"),
    ("sql_statement_not_yet_complete", "03000"),
    ("connection_exception", "08000"),
    ("connection_does_not_exist", "08003"),
    ("connection_failure", "08006"),
    ("sqlclient_unable_to_establish_sqlconnection", "08001"),
    ("sqlserver_rejected_establishment_of_sqlconnection", "08004"),
    ("transaction_resolution_unknown", "08007"),
    ("protocol_violation", "08P01"),
    ("triggered_action_exception", "09000"),
    ("feature_not_supported", "0A000"),
    ("invalid_transaction_initiation", "0B000"),
    ("locator_exception", "0F000"),
    ("invalid_locator_specification", "0F001"),
    ("invalid_grantor", "0L000"),
    ("invalid_grant_operation", "0LP01"),
    ("invalid_role_specification", "0P000"),
    ("diagnostics_exception", "0Z000"),
    (
        "stacked_diagnostics_accessed_without_active_handler",
        "0Z002",
    ),
    ("case_not_found", "20000"),
    ("cardinality_violation", "21000"),
    ("data_exception", "22000"),
    ("array_subscript_error", "2202E"),
    ("character_not_in_repertoire", "22021"),
    ("datetime_field_overflow", "22008"),
    ("division_by_zero", "22012"),
    ("error_in_assignment", "22005"),
    ("escape_character_conflict", "2200B"),
    ("indicator_overflow", "22022"),
    ("interval_field_overflow", "22015"),
    ("invalid_argument_for_logarithm", "2201E"),
    ("invalid_argument_for_ntile_function", "22014"),
    ("invalid_argument_for_nth_value_function", "22016"),
    ("invalid_argument_for_power_function", "2201F"),
    ("invalid_argument_for_width_bucket_function", "2201G"),
    ("invalid_character_value_for_cast", "22018"),
    ("invalid_datetime_format", "22007"),
    ("invalid_escape_character", "22019"),
    ("invalid_escape_octet", "2200D"),
    ("invalid_escape_sequence", "22025"),
    ("nonstandard_use_of_escape_character", "22P06"),
    ("invalid_indicator_parameter_value", "22010"),
    ("invalid_parameter_value", "22023"),
    ("invalid_preceding_or_following_size", "22013"),
    ("invalid_regular_expression", "2201B"),
    ("invalid_row_count_in_limit_clause", "2201W"),
    ("invalid_row_count_in_result_offset_clause", "2201X"),
    ("invalid_tablesample_argument", "2202H"),
    ("invalid_tablesample_repeat", "2202G"),
    ("invalid_time_zone_displacement_value", "22009"),
    ("invalid_use_of_escape_character", "2200C"),
    ("most_specific_type_mismatch", "2200G"),
    ("null_value_not_allowed", "22004"),
    ("null_value_no_indicator_parameter", "22002"),
    ("numeric_value_out_of_range", "22003"),
    ("sequence_generator_limit_exceeded", "2200H"),
    ("string_data_length_mismatch", "22026"),
    ("substring_error", "22011"),
    ("trim_error", "22027"),
    ("unterminated_c_string", "22024"),
    ("zero_length_character_string", "2200F"),
    ("floating_point_exception", "22P01"),
    ("invalid_text_representation", "22P02"),
    ("invalid_binary_representation", "22P03"),
    ("bad_copy_file_format", "22P04"),
    ("untranslatable_character", "22P05"),
    ("not_an_xml_document", "2200L"),
    ("invalid_xml_document", "2200M"),
    ("invalid_xml_content", "2200N"),
    ("invalid_xml_comment", "2200S"),
    ("invalid_xml_processing_instruction", "2200T"),
    ("duplicate_json_object_key_value", "22030"),
    ("invalid_argument_for_sql_json_datetime_function", "22031"),
    ("invalid_json_text", "22032"),
    ("invalid_sql_json_subscript", "22033"),
    ("more_than_one_sql_json_item", "22034"),
    ("no_sql_json_item", "22035"),
    ("non_numeric_sql_json_item", "22036"),
    ("non_unique_keys_in_a_json_object", "22037"),
    ("singleton_sql_json_item_required", "22038"),
    ("sql_json_array_not_found", "22039"),
    ("sql_json_member_not_found", "2203A"),
    ("sql_json_number_not_found", "2203B"),
    ("sql_json_object_not_found", "2203C"),
    ("too_many_json_array_elements", "2203D"),
    ("too_many_json_object_members", "2203E"),
    ("sql_json_scalar_required", "2203F"),
    ("sql_json_item_cannot_be_cast_to_target_type", "2203G"),
    ("integrity_constraint_violation", "23000"),
    ("restrict_violation", "23001"),
    ("not_null_violation", "23502"),
    ("foreign_key_violation", "23503"),
    ("unique_violation", "23505"),
    ("check_violation", "23514"),
    ("exclusion_violation", "23P01"),
    ("invalid_cursor_state", "24000"),
    ("invalid_transaction_state", "25000"),
    ("active_sql_transaction", "25001"),
    ("branch_transaction_already_active", "25002"),
    ("held_cursor_requires_same_isolation_level", "25008"),
    ("inappropriate_access_mode_for_branch_transaction", "25003"),
    (
        "inappropriate_isolation_level_for_branch_transaction",
        "25004",
    ),
    ("no_active_sql_transaction_for_branch_transaction", "25005"),
    ("read_only_sql_transaction", "25006"),
    ("schema_and_data_statement_mixing_not_supported", "25007"),
    ("no_active_sql_transaction", "25P01"),
    ("in_failed_sql_transaction", "25P02"),
    ("idle_in_transaction_session_timeout", "25P03"),
    ("invalid_sql_statement_name", "26000"),
    ("triggered_data_change_violation", "27000"),
    ("invalid_authorization_specification", "28000"),
    ("invalid_password", "28P01"),
    ("dependent_privilege_descriptors_still_exist", "2B000"),
    ("dependent_objects_still_exist", "2BP01"),
    ("invalid_transaction_termination", "2D000"),
    ("sql_routine_exception", "2F000"),
    ("function_executed_no_return_statement", "2F005"),
    ("modifying_sql_data_not_permitted", "2F002"),
    ("prohibited_sql_statement_attempted", "2F003"),
    ("reading_sql_data_not_permitted", "2F004"),
    ("invalid_cursor_name", "34000"),
    ("external_routine_exception", "38000"),
    ("containing_sql_not_permitted", "38001"),
    ("modifying_sql_data_not_permitted", "38002"),
    ("prohibited_sql_statement_attempted", "38003"),
    ("reading_sql_data_not_permitted", "38004"),
    ("external_routine_invocation_exception", "39000"),
    ("invalid_sqlstate_returned", "39001"),
    ("null_value_not_allowed", "39004"),
    ("trigger_protocol_violated", "39P01"),
    ("srf_protocol_violated", "39P02"),
    ("event_trigger_protocol_violated", "39P03"),
    ("savepoint_exception", "3B000"),
    ("invalid_savepoint_specification", "3B001"),
    ("invalid_catalog_name", "3D000"),
    ("invalid_schema_name", "3F000"),
    ("transaction_rollback", "40000"),
    ("transaction_integrity_constraint_violation", "40002"),
    ("serialization_failure", "40001"),
    ("statement_completion_unknown", "40003"),
    ("deadlock_detected", "40P01"),
    ("syntax_error_or_access_rule_violation", "42000"),
    ("syntax_error", "42601"),
    ("insufficient_privilege", "42501"),
    ("cannot_coerce", "42846"),
    ("grouping_error", "42803"),
    ("windowing_error", "42P20"),
    ("invalid_recursion", "42P19"),
    ("invalid_foreign_key", "42830"),
    ("invalid_name", "42602"),
    ("name_too_long", "42622"),
    ("reserved_name", "42939"),
    ("datatype_mismatch", "42804"),
    ("indeterminate_datatype", "42P18"),
    ("collation_mismatch", "42P21"),
    ("indeterminate_collation", "42P22"),
    ("wrong_object_type", "42809"),
    ("generated_always", "428C9"),
    ("undefined_column", "42703"),
    ("undefined_function", "42883"),
    ("undefined_table", "42P01"),
    ("undefined_parameter", "42P02"),
    ("undefined_object", "42704"),
    ("duplicate_column", "42701"),
    ("duplicate_cursor", "42P03"),
    ("duplicate_database", "42P04"),
    ("duplicate_function", "42723"),
    ("duplicate_prepared_statement", "42P05"),
    ("duplicate_schema", "42P06"),
    ("duplicate_table", "42P07"),
    ("duplicate_alias", "42712"),
    ("duplicate_object", "42710"),
    ("ambiguous_column", "42702"),
    ("ambiguous_function", "42725"),
    ("ambiguous_parameter", "42P08"),
    ("ambiguous_alias", "42P09"),
    ("invalid_column_reference", "42P10"),
    ("invalid_column_definition", "42611"),
    ("invalid_cursor_definition", "42P11"),
    ("invalid_database_definition", "42P12"),
    ("invalid_function_definition", "42P13"),
    ("invalid_prepared_statement_definition", "42P14"),
    ("invalid_schema_definition", "42P15"),
    ("invalid_table_definition", "42P16"),
    ("invalid_object_definition", "42P17"),
    ("with_check_option_violation", "44000"),
    ("insufficient_resources", "53000"),
    ("disk_full", "53100"),
    ("out_of_memory", "53200"),
    ("too_many_connections", "53300"),
    ("configuration_limit_exceeded", "53400"),
    ("program_limit_exceeded", "54000"),
    ("statement_too_complex", "54001"),
    ("too_many_columns", "54011"),
    ("too_many_arguments", "54023"),
    ("object_not_in_prerequisite_state", "55000"),
    ("object_in_use", "55006"),
    ("cant_change_runtime_param", "55P02"),
    ("lock_not_available", "55P03"),
    ("unsafe_new_enum_value_usage", "55P04"),
    ("operator_intervention", "57000"),
    ("query_canceled", "57014"),
    ("admin_shutdown", "57P01"),
    ("crash_shutdown", "57P02"),
    ("cannot_connect_now", "57P03"),
    ("database_dropped", "57P04"),
    ("idle_session_timeout", "57P05"),
    ("system_error", "58000"),
    ("io_error", "58030"),
    ("undefined_file", "58P01"),
    ("duplicate_file", "58P02"),
    ("snapshot_too_old", "72000"),
    ("config_file_error", "F0000"),
    ("lock_file_exists", "F0001"),
    ("fdw_error", "HV000"),
    ("plpgsql_error", "P0000"),
    ("raise_exception", "P0001"),
    ("no_data_found", "P0002"),
    ("too_many_rows", "P0003"),
    ("assert_failure", "P0004"),
    ("internal_error", "XX000"),
    ("data_corrupted", "XX001"),
    ("index_corrupted", "XX002"),
];

/// 条件名（大文字小文字を区別しない）に対応する SQLSTATE の一覧。未知の名前は空。
pub(crate) fn codes_for_name(name: &str) -> Vec<&'static str> {
    let lower = name.to_ascii_lowercase();
    CONDITIONS
        .iter()
        .filter(|(n, _)| *n == lower)
        .map(|(_, c)| *c)
        .collect()
}

/// `RAISE condition_name` が使う SQLSTATE（表の先頭）。未知の名前は `None`。
pub(crate) fn raise_code_for_name(name: &str) -> Option<&'static str> {
    codes_for_name(name).first().copied()
}

/// SQLSTATE の文字列 `s` を検証する（5 文字・`[0-9A-Z]`。実装定義クラス `VD` は `42601`）。
/// 構文段（`SQLSTATE '...'`）と実行時（`ERRCODE` の式）が共有する。
pub(crate) fn validate_sqlstate_text(s: &str) -> Result<String, SqlSurfaceError> {
    if !crate::notice::is_valid_sqlstate(s) || s.starts_with("VD") {
        return Err(SqlSurfaceError::unsupported("invalid SQLSTATE code"));
    }
    Ok(s.to_string())
}

/// `ERRCODE` の値（5 文字コードまたは条件名）を SQLSTATE へ解決する。未知の条件名は `42704`。
pub(crate) fn resolve_errcode(text: &str) -> Result<String, SqlSurfaceError> {
    if crate::notice::is_valid_sqlstate(text) {
        return validate_sqlstate_text(text);
    }
    match raise_code_for_name(text) {
        Some(c) => Ok(c.to_string()),
        None => Err(SqlSurfaceError::UndefinedObject {
            name: "exception condition".to_string(),
        }),
    }
}

/// `EXCEPTION WHEN` の 1 条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExcCond {
    /// 5 文字の SQLSTATE（末尾が `000` ならクラス全体に一致するカテゴリ）。
    Code(String),
    /// `OTHERS`（`57014`＝`query_canceled` と `P0004`＝`assert_failure` 以外の全て）。
    Others,
}

/// `code`（5 文字の SQLSTATE）が条件 `cond` に一致するか。`Code` はコードの完全一致に加え、条件が
/// カテゴリ（末尾 `000`）なら同じクラス（先頭 2 文字）のエラーに一致する。
pub(crate) fn matches_code(cond: &ExcCond, code: &str) -> bool {
    match cond {
        ExcCond::Others => code != "57014" && code != "P0004",
        ExcCond::Code(c) => {
            if c == code {
                return true;
            }
            match (c.strip_suffix("000"), code.get(..2)) {
                (Some(class), Some(prefix)) if class.len() == 2 => class == prefix,
                _ => false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_lookup_is_case_insensitive_and_multi_code() {
        assert_eq!(codes_for_name("UNIQUE_VIOLATION"), vec!["23505"]);
        assert_eq!(
            codes_for_name("string_data_right_truncation"),
            vec!["01004", "22001"]
        );
        assert!(codes_for_name("no_such_condition").is_empty());
        assert_eq!(
            raise_code_for_name("string_data_right_truncation"),
            Some("01004")
        );
    }

    #[test]
    fn category_matches_whole_class() {
        let cat = ExcCond::Code("22000".to_string());
        assert!(matches_code(&cat, "22012"));
        assert!(matches_code(&cat, "22000"));
        assert!(!matches_code(&cat, "23505"));
        let exact = ExcCond::Code("22012".to_string());
        assert!(matches_code(&exact, "22012"));
        assert!(!matches_code(&exact, "22003"));
    }

    #[test]
    fn others_excludes_query_canceled_and_assert_failure() {
        assert!(matches_code(&ExcCond::Others, "23505"));
        assert!(matches_code(&ExcCond::Others, "P0001"));
        assert!(!matches_code(&ExcCond::Others, "57014"));
        assert!(!matches_code(&ExcCond::Others, "P0004"));
        assert!(matches_code(&ExcCond::Code("57014".into()), "57014"));
    }
}
