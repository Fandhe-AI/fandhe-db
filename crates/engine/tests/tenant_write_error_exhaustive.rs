//! `TenantWriteError` への `#[non_exhaustive]` 後付け不採用（Issue #282・
//! `docs/design/error-enum-non-exhaustive-policy.md`）をコンパイル時にピン留めする
//! 結合テスト（対象ビヘイビア: ERR-2 のポインタ。`docs/spec/04-behavior/error-format.md`）。
//!
//! `engine::tenant::TenantWriteError` を **クレート外**（本ファイルは別クレート扱いの
//! 結合テスト）から `_` アームなしで網羅的に `match` する。この網羅 `match` は
//! 次の 2 通りいずれでもコンパイルエラーになる:
//!
//! - 将来 `TenantWriteError` に `#[non_exhaustive]` が付与された場合（クレート外の
//!   網羅的 `match` はコンパイル不能になる仕様）
//! - 将来 `TenantWriteError` に variant が追加された場合（本ファイルの `match` を
//!   更新しない限り網羅性エラーになる）
//!
//! いずれの変更も ADR の再検討（`#[non_exhaustive]` を付けない方針の見直し）を
//! 要求する形になっており、方針変更が静かに紛れ込むのを防ぐ。

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::sql::allowlist::SqlSurfaceError;
use engine::tenant::TenantWriteError;

/// `TenantWriteError` の全 variant を `_` アームなしで分類し、期待される
/// [`ErrorClass`] を返す。`wire_code` の具体値は写像先ではなく
/// `crate::error_format::ErrorClass`（TASK-152・ERR-2 の単一真実源）が持つため、
/// ここでは分類（variant → `ErrorClass`）だけをピン留めし、`wire_code` の文字列を
/// 二重管理しない（写像の値そのものは、`Forbidden`/`NotFound`/`IdConflict`/
/// `DuplicateOperationId`/`MissingOperationId` の 5 variant を
/// `crates/engine/tests/error_format.rs` の `err2_tenant_write_error_mapping_is_unchanged`
/// が、残る `OperationIdContentMismatch` の `wire_code`（`22023`）を
/// `crates/engine/tests/recovery_content_hash.rs` が別途検証する）。
fn expected_class(e: &TenantWriteError) -> ErrorClass {
    match e {
        TenantWriteError::Forbidden => ErrorClass::ForbiddenTenantMismatch,
        TenantWriteError::ConflictTargetNotVisible => ErrorClass::ForbiddenTenantMismatch,
        TenantWriteError::NotFound => ErrorClass::RowNotFound,
        TenantWriteError::IdConflict => ErrorClass::UniqueViolation,
        TenantWriteError::MissingOperationId => ErrorClass::MissingOperationId,
        TenantWriteError::Catalog(_) => ErrorClass::InternalError,
        TenantWriteError::Storage(_) => ErrorClass::InternalError,
        TenantWriteError::LedgerCorrupted(_) => ErrorClass::InternalError,
        TenantWriteError::DuplicateOperationId => ErrorClass::DuplicateOperationId,
        TenantWriteError::OperationIdContentMismatch => ErrorClass::OperationIdContentMismatch,
        TenantWriteError::ReturningProjectionFailed(_) => ErrorClass::InternalError,
        TenantWriteError::ReturningProjectionTooLarge(_) => ErrorClass::PayloadTooLarge,
        TenantWriteError::CapturedRowDecodeFailed(_) => ErrorClass::InternalError,
        TenantWriteError::TooManyRowsScanned => ErrorClass::PayloadTooLarge,
        TenantWriteError::UniqueViolation => ErrorClass::UniqueViolation,
        TenantWriteError::CheckViolation { .. } => ErrorClass::CheckViolation,
        // オーナー判断（2026-09-28・Issue #1075）: 内側の `SqlSurfaceError` の
        // 分類（通常の式評価と同じ）へ委譲する。`XX000` 固定ではなくなった。
        TenantWriteError::CheckEvaluationFailed(inner) => inner.error_class(),
        TenantWriteError::ForeignKeyViolation => ErrorClass::ForeignKeyViolation,
        TenantWriteError::ReferentialActionLimitExceeded => ErrorClass::PayloadTooLarge,
        TenantWriteError::WriteLockTimeout => ErrorClass::LockNotAvailable,
    }
}

/// 構築可能な variant（内部にエラー型を保持しない variant）について、
/// `expected_class` と実装済み `error_class()` が一致することを確認する。
/// `Catalog`/`Storage`/`LedgerCorrupted` の分類一致は
/// `crates/engine/tests/error_format.rs` が別途カバーするため、ここでは
/// 網羅 `match`（本ファイルの `expected_class`）のコンパイル固定が主目的。
#[test]
fn tenant_write_error_class_matches_expected_for_constructible_variants() {
    let cases = [
        TenantWriteError::Forbidden,
        TenantWriteError::NotFound,
        TenantWriteError::IdConflict,
        TenantWriteError::MissingOperationId,
        TenantWriteError::DuplicateOperationId,
        TenantWriteError::OperationIdContentMismatch,
        TenantWriteError::ReturningProjectionFailed("test".to_string()),
        TenantWriteError::ReturningProjectionTooLarge("test".to_string()),
        TenantWriteError::CapturedRowDecodeFailed("test".to_string()),
        TenantWriteError::TooManyRowsScanned,
        TenantWriteError::UniqueViolation,
        TenantWriteError::ForeignKeyViolation,
        TenantWriteError::ReferentialActionLimitExceeded,
        TenantWriteError::CheckViolation {
            constraint: "test_check".to_string(),
        },
        // 内側の `SqlSurfaceError` の分類（`InvalidInput`＝`22000`・`Internal`＝
        // `XX000`）の両方で `error_class()` の委譲が正しく効くことを固定する。
        TenantWriteError::CheckEvaluationFailed(SqlSurfaceError::InvalidInput {
            detail: "division by zero".to_string(),
        }),
        TenantWriteError::CheckEvaluationFailed(SqlSurfaceError::Internal {
            detail: "test".to_string(),
        }),
        TenantWriteError::WriteLockTimeout,
        TenantWriteError::ConflictTargetNotVisible,
    ];
    for case in &cases {
        assert_eq!(
            expected_class(case),
            case.error_class(),
            "error_class mismatch for variant"
        );
    }
}
