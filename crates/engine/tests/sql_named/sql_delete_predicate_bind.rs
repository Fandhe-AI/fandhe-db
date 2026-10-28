//! `validate_delete_statement`／`bind_predicate_delete`／`ValidatedPredicateDelete`／
//! `BoundPredicateDelete`（Issue #870・TASK-192・SQL-19）が engine クレート外から
//! 到達可能な公開 API であることを固定する結合テスト
//! （`tests/sql_scan_public_api.rs`・`tests/sql_insert_explain_public_api.rs` と
//! 同じ流儀）。
//!
//! 実行結線（可視行列挙・1 トランザクション一括適用・影響行数上限の実測判定・
//! 台帳照合）は Issue #871 の担当であり、本ファイルはその前段（許可リスト検証・
//! 束縛）までが engine クレート外から完結して呼べることのみを固定する。あわせて
//! `EngineCore::execute_sql_in_session` が `DELETE` 文を引き続き `42601` で拒否する
//! こと（#871 が dispatch を結線するまでの現状固定。`tests/sql_insert_session_dispatch.rs`
//! と同じ「セッション経由の分岐がまだ存在しない」ことの確認）もあわせて検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::allowlist::{validate_delete_statement, DeleteStatement};
use engine::sql::mode::SessionState;
use engine::sql::parser::bind_predicate_delete;
use engine::sql::udf_call::UdfRegistry;
use engine::storage::Storage;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn new_storage_with_documents_table(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage
}

/// `validate_delete_statement` → `bind_predicate_delete` が engine クレート外
/// から `pub` 関数のみで完結して呼べることを固定する（第 2 の述語実装を
/// 作らない契約の到達性確認）。
#[test]
fn validate_delete_statement_and_bind_predicate_delete_are_reachable_from_outside_the_crate() {
    let path = unique_db_path("delete-predicate-bind-reachable");
    let _guard = CleanupGuard(path.clone());
    let storage = new_storage_with_documents_table(&path);

    let stmt = validate_delete_statement(
        "DELETE FROM documents WHERE lang = 'ja' AND id > 5 USING OPERATION_ID 'op-0001'",
        &storage,
        LedgerMode::Ledgered,
    )
    .expect("predicate DELETE should pass the allowlist");
    let predicate = match stmt {
        DeleteStatement::Predicate(pd) => pd,
        DeleteStatement::SingleRow(_) => panic!("must classify as predicate form"),
    };
    assert_eq!(predicate.table_name(), TABLE);
    assert_eq!(predicate.where_predicates().len(), 2);
    assert_eq!(
        predicate.operation_id().map(|id| id.as_str()),
        Some("op-0001")
    );

    let table_schema = storage.get_table_schema(TABLE).expect("schema lookup");
    let bound = bind_predicate_delete(&predicate, &table_schema, &UdfRegistry::default())
        .expect("bind_predicate_delete should succeed");
    assert_eq!(bound.table(), TABLE);
    assert_eq!(bound.metadata_filters().len(), 1);
    assert_eq!(bound.expr_filters().len(), 1);
    assert_eq!(bound.operation_id().map(|id| id.as_str()), Some("op-0001"));
}

/// 単一行・`id` 完全一致形は `validate_delete_statement` 経由でも従来どおり
/// `DeleteStatement::SingleRow` へ分類される（既存 SQL-18 の受理範囲を変えない）。
#[test]
fn validate_delete_statement_still_classifies_single_row_form() {
    let path = unique_db_path("delete-predicate-bind-single-row");
    let _guard = CleanupGuard(path.clone());
    let storage = new_storage_with_documents_table(&path);

    let stmt = validate_delete_statement(
        "DELETE FROM documents WHERE id = 1 USING OPERATION_ID 'op-0001'",
        &storage,
        LedgerMode::Ledgered,
    )
    .expect("single-row DELETE should pass the allowlist");
    assert!(matches!(stmt, DeleteStatement::SingleRow(_)));
}

fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
    let storage = new_storage_with_documents_table(path);
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

/// #871 が dispatch を結線するまで、`DELETE`（単一行・述語形いずれも）は
/// `execute_sql_in_session` 経由では実行できないことを明示的に固定する
/// （`sql::allowlist::validate_sql_still_rejects_delete_statement` の
/// セッション経由版）。
///
/// Issue #871 で `execute_sql_in_session` の `DELETE` 分岐が述語形まで実行結線
/// されたため、本テストは「まだ拒否される」固定から「0 件一致で成功する」
/// 固定へ反転する（テーブルに行が無いため `rows_affected == 0`。台帳は
/// commit 済みで、同一 `operation_id` の再送は `23505` に収束する契約
/// （`docs/design/predicate-dml-exec.md` 参照）は `tests/sql_predicate_dml_exec.rs`
/// が固定する）。
#[test]
fn session_executes_predicate_delete_statement() {
    let path = unique_db_path("delete-predicate-bind-session-executes");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let mut session = SessionState::default();

    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "DELETE FROM documents WHERE lang = 'ja' USING OPERATION_ID 'op-0001'",
        )
        .expect("session DELETE (predicate form) must now execute");
    match outcome {
        engine::sql::SqlOutcome::Delete(o) => assert_eq!(o.rows_affected, 0),
        other => panic!("expected SqlOutcome::Delete, got {other:?}"),
    }
}
