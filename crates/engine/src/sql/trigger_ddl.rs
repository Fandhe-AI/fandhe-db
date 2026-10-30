//! `CREATE [OR REPLACE] TRIGGER`／`DROP TRIGGER` の実行本体（TABLE-45、Issue #1722。設計記録:
//! `docs/design/trigger-catalog.md`）。
//!
//! 役割: 構造検証済みの文（`sql::allowlist::{ValidatedCreateTrigger, ValidatedDropTrigger}`）を TRIGGER
//! カタログ（`catalog::trigger`）への書き込みへ写し、[`CatalogError`] を SQLSTATE の契約へ写像する。
//! 呼び出し元は `core.rs` の `ParsedSql::CreateTrigger`／`DropTrigger` 腕で、DDL 権限ゲート
//! （`sql::ddl::require_ddl_permission`。権限なしは対象の有無を問わず `42501`）と明示トランザクション内の
//! `0A000` を通過した後にだけ来る。
//!
//! 判定順（決定的・fail-closed）: 構文（`allowlist`）→ 権限 → 明示 Tx → カタログの単一 write txn 内で
//! 対象の種別（ビュー等は `42809`）→ 表の存在（`42P01`）→ `UPDATE OF` の列（`42703`）→ 関数
//! （不在 `42883`・トリガ関数でない `42P17`）→ 同名の既存（`42710`）→ 件数上限（`54000`）。
//!
//! テナント境界（P0）: scope は呼び出し元が渡す検証済みの `tenant_id` だけ。他テナントのトリガは不在と
//! 同じ経路・同じ `42704` になる。エラー文言は固定で、内部キー・tenant_id・他テナントの定義を含めない。

use crate::catalog::trigger::{
    TriggerDef, TriggerEvents, TRIGGER_ALREADY_EXISTS_MESSAGE, TRIGGER_FUNCTION_NOT_FOUND_MESSAGE,
    TRIGGER_FUNCTION_NOT_TRIGGER_MESSAGE,
};
use crate::catalog::CatalogError;
use crate::sql::allowlist::{SqlSurfaceError, ValidatedCreateTrigger, ValidatedDropTrigger};
use crate::storage::Storage;

/// `CREATE TRIGGER` の成功応答（結果を持たない。wire はコマンドタグ `CREATE TRIGGER` を返す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CreateTriggerOutcome {}

/// `DROP TRIGGER` の成功応答。`IF EXISTS` で対象が無く no-op になったかだけを持つ
/// （CommandComplete のタグは通常の DROP と同一のため、wire 側は参照しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DropTriggerOutcome {
    pub(crate) skipped: bool,
}

impl DropTriggerOutcome {
    /// `IF EXISTS` により対象不在で no-op 成功した場合に真。
    pub fn skipped(&self) -> bool {
        self.skipped
    }
}

/// `CREATE [OR REPLACE] TRIGGER` を実行する（権限・明示 Tx のゲートは呼び出し元で通過済み）。
///
/// `RETURNS trigger` 関数の保存形式（#2228）が入るまで、実在する関数でも `42P17` になり成功経路には
/// 到達しない（`catalog::trigger::returns_trigger`。fail-closed）。
pub(crate) fn execute_create_trigger(
    storage: &Storage,
    tenant_id: &str,
    stmt: &ValidatedCreateTrigger,
) -> Result<CreateTriggerOutcome, SqlSurfaceError> {
    let def = TriggerDef {
        id: 0,
        name: stmt.name.clone(),
        timing: stmt.timing,
        events: TriggerEvents::from_bits(stmt.events)
            .map_err(|e| map_trigger_error(e, &stmt.name))?,
        level: stmt.level,
        update_of: stmt.update_of.clone(),
        function_key: stmt.function.clone(),
        tg_args: stmt.tg_args.clone(),
        when_sql: None,
    };
    storage
        .create_trigger(tenant_id, &stmt.table, &def, stmt.or_replace)
        .map_err(|e| map_trigger_error(e, &stmt.name))?;
    Ok(CreateTriggerOutcome {})
}

/// `DROP TRIGGER` を実行する。対象が無い場合（表が無い・他テナントにしか無い場合を含む）は `IF EXISTS`
/// なら成功の no-op、そうでなければ表の不在は `42P01`、トリガの不在は `42704`。
pub(crate) fn execute_drop_trigger(
    storage: &Storage,
    tenant_id: &str,
    stmt: &ValidatedDropTrigger,
) -> Result<DropTriggerOutcome, SqlSurfaceError> {
    match storage.drop_trigger(tenant_id, &stmt.table, &stmt.name) {
        Ok(true) => Ok(DropTriggerOutcome { skipped: false }),
        Ok(false) | Err(CatalogError::TableNotFound(_)) if stmt.if_exists => {
            Ok(DropTriggerOutcome { skipped: true })
        }
        Ok(false) => Err(SqlSurfaceError::UndefinedObject {
            name: stmt.name.clone(),
        }),
        Err(e) => Err(map_trigger_error(e, &stmt.name)),
    }
}

/// TRIGGER カタログの [`CatalogError`] を SQL 表層の契約へ写像する（同名の既存 `42710`・関数不在 `42883`・
/// トリガ関数でない `42P17`・表不在 `42P01`・種別違い `42809`・列不在 `42703`・件数上限 `54000`・
/// 形状不正 `22000`・ロック timeout `55P03`・その他 `XX000`）。文言は固定で、内部キー・tenant_id・
/// 他テナントの定義を含めない（security.md P0）。`trigger_name` は利用者自身が書いた名前。
fn map_trigger_error(e: CatalogError, trigger_name: &str) -> SqlSurfaceError {
    match e {
        CatalogError::Invalid(m) if m == TRIGGER_ALREADY_EXISTS_MESSAGE => {
            SqlSurfaceError::duplicate_object(trigger_name)
        }
        CatalogError::Invalid(m) if m == TRIGGER_FUNCTION_NOT_FOUND_MESSAGE => {
            SqlSurfaceError::undefined_function("function does not exist")
        }
        CatalogError::Invalid(m) if m == TRIGGER_FUNCTION_NOT_TRIGGER_MESSAGE => {
            SqlSurfaceError::InvalidObjectDefinition {
                detail: "function must return type trigger",
            }
        }
        CatalogError::Invalid(_) => SqlSurfaceError::invalid_input("invalid trigger definition"),
        CatalogError::ViewLimitExceeded(_) => {
            SqlSurfaceError::payload_too_large("too many triggers")
        }
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::ColumnNotFound(name) => SqlSurfaceError::undefined_column(name),
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "trigger catalog operation failed".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_errors_map_to_fixed_sqlstates() {
        let code = |e| map_trigger_error(e, "g").wire_code();
        assert_eq!(
            code(CatalogError::Invalid(
                TRIGGER_ALREADY_EXISTS_MESSAGE.to_string()
            )),
            "42710"
        );
        assert_eq!(
            code(CatalogError::Invalid(
                TRIGGER_FUNCTION_NOT_FOUND_MESSAGE.to_string()
            )),
            "42883"
        );
        assert_eq!(
            code(CatalogError::Invalid(
                TRIGGER_FUNCTION_NOT_TRIGGER_MESSAGE.to_string()
            )),
            "42P17"
        );
        assert_eq!(code(CatalogError::Invalid("x".to_string())), "22000");
        assert_eq!(
            code(CatalogError::ViewLimitExceeded("x".to_string())),
            "54000"
        );
        assert_eq!(code(CatalogError::TableNotFound("t".to_string())), "42P01");
        assert_eq!(
            code(CatalogError::WrongObjectKind("t".to_string())),
            "42809"
        );
        assert_eq!(code(CatalogError::ColumnNotFound("c".to_string())), "42703");
        assert_eq!(code(CatalogError::WriteLockTimeout), "55P03");
        let corrupt =
            map_trigger_error(CatalogError::CorruptSchema("secret #1.f".to_string()), "g");
        assert_eq!(corrupt.wire_code(), "XX000");
        assert!(!corrupt.client_message().contains("secret"));
    }
}
