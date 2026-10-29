//! `CREATE FUNCTION`／`DROP FUNCTION` の永続化経路（SQL-66・TASK-309、Issue #1920。親: #1918。
//! 設計記録: `docs/design/udf-catalog.md`「書き込み経路」）。
//!
//! 役割: セッションの [`UdfRegistry`] だけにあった UDF 定義を UDF カタログ（`catalog::function`）へ
//! 書く・消す実行本体と、[`CatalogError`] → SQLSTATE の写像を担う。呼び出し元は `core.rs` の
//! `execute_validated_in_session`（`CREATE FUNCTION`）と `ParsedSql::DropFunction` 腕で、どちらも
//! `session.ddl_allowed()` を満たす場合だけここへ来る（権限ゲートの 42501 化は #1922）。
//!
//! 順序の契約: 検証（レジストリ不変）→ カタログの単一 write txn・1 回の commit → commit 成功後に
//! レジストリへ反映。カタログで失敗したらセッションは変わらない。クラッシュしても commit 境界で
//! 全か無かになる（`recovery::commit_boundary` 経由）。
//!
//! テナント境界（P0）: scope は呼び出し元が渡す検証済みの `tenant_id` だけ。他テナントの関数は不在と
//! 同じ経路・同じ `42883` になる。エラー文言は固定で、内部キー（`#<id>.<name>`）・tenant_id・本体の
//! 原文を含めない。

use crate::catalog::function::{
    FunctionAttributes, FunctionDef, FunctionLanguage, FunctionParam, FunctionType,
    FUNCTION_ALREADY_EXISTS_MESSAGE, FUNCTION_AMBIGUOUS_MESSAGE,
};
use crate::catalog::CatalogError;
use crate::sql::allowlist::{SqlSurfaceError, ValidatedDropFunction};
use crate::sql::mode::SessionState;
use crate::sql::udf_call::{commit_function_definition, prepare_function_definition};
use crate::storage::Storage;

/// `DROP FUNCTION` の成功応答（応答は結果を持たない。wire はコマンドタグ `DROP FUNCTION` を返す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropFunctionOutcome {}

/// `CREATE FUNCTION` をカタログへ永続化してセッションへ登録する（DDL 権限のあるセッション用）。
///
/// 判定順は [`prepare_function_definition`]（予約名・識別子・引数・閉じた本体。セッションの重複
/// `42723` を含む）→ [`Storage::create_function`]（同名の定義があれば `42723`・件数上限 `54000`）→
/// セッションへの登録。
pub(crate) fn execute_create_function_persistent(
    storage: &Storage,
    tenant_id: &str,
    session: &mut SessionState,
    name: &str,
    params: &[String],
    body: &crate::sql::udf_call::Expr,
    body_source: &str,
) -> Result<(), SqlSurfaceError> {
    let prepared = prepare_function_definition(session.udfs(), name, params, body)?;
    let def = FunctionDef {
        id: 1,
        function_key: prepared.key.clone(),
        // 引数の型注釈は未対応（宣言的 UDF〔SQL-9〕は型なし）。型付き引数は #1915 が足す。
        params: prepared
            .params
            .iter()
            .map(|p| FunctionParam {
                name: Some(p.clone()),
                ty: None::<FunctionType>,
            })
            .collect(),
        return_type: None,
        language: FunctionLanguage::Declarative,
        body_source: body_source.to_string(),
        attrs: FunctionAttributes::default(),
    };
    storage
        .create_function(tenant_id, &def)
        .map_err(map_function_error)?;
    commit_function_definition(session.udfs_mut(), prepared);
    Ok(())
}

/// `DROP FUNCTION` を実行する。DDL 権限のあるセッションはカタログ（自テナントの scope）から削除し、
/// いずれのセッションもレジストリから削除する。**どちらでも消えなかった場合だけ** `42883`
/// （存在情報を出さない固定文言）。権限の無いセッションはカタログを見ない。
///
/// 型の並びが指定されたときは、カタログ上の定義の引数の数と（型注釈のある引数の）型が一致する
/// 場合だけ削除する（型注釈なしの引数は任意の型に一致する）。不一致は不在と同じ `42883`。
pub(crate) fn execute_drop_function(
    storage: &Storage,
    tenant_id: &str,
    session: &mut SessionState,
    stmt: &ValidatedDropFunction,
) -> Result<DropFunctionOutcome, SqlSurfaceError> {
    let mut removed = false;
    if session.ddl_allowed() {
        removed |= drop_from_catalog(storage, tenant_id, stmt)?;
    }
    // 型の並びがある場合、セッション内の定義（型注釈なし）は引数の数で照合する。
    // WASM UDF は (vector, scalar) の固定 2 引数なので、同じ名前空間を共有する
    // `UdfRegistry::remove` が両表を消す設計に合わせて判定にも含める。
    let session_match = {
        let udfs = session.udfs();
        let arity_ok = |n: usize| stmt.arg_types.as_ref().is_none_or(|types| types.len() == n);
        udfs.get(&stmt.name)
            .is_some_and(|d| arity_ok(d.params.len()))
            || udfs.get_wasm(&stmt.name).is_some_and(|_| arity_ok(2))
    };
    if session_match {
        removed |= session.udfs_mut().remove(&stmt.name);
    }
    if removed {
        Ok(DropFunctionOutcome {})
    } else {
        Err(SqlSurfaceError::undefined_function(
            "function does not exist",
        ))
    }
}

/// カタログから削除し、削除したら `true`。型付きの場合も照合と削除は同一 write txn で行う。
fn drop_from_catalog(
    storage: &Storage,
    tenant_id: &str,
    stmt: &ValidatedDropFunction,
) -> Result<bool, SqlSurfaceError> {
    let Some(types) = &stmt.arg_types else {
        return storage
            .drop_function_by_name(tenant_id, &stmt.name)
            .map(|sig| sig.is_some())
            .map_err(map_function_error);
    };
    // 列挙・署名照合・削除を 1 つの write txn で行う（読みと削除の間に別セッションの
    // drop／再作成が割り込んで新しい定義を誤って消さないため）。
    storage
        .drop_function_matching(tenant_id, &stmt.name, |def| {
            if def.params.len() != types.len() {
                return Ok(false);
            }
            for (stored, want) in def.params.iter().zip(types) {
                let Some(stored_ty) = &stored.ty else {
                    continue;
                };
                let want_ty = FunctionType::from_column_type(want)?;
                if stored_ty.identity_token() != want_ty.identity_token() {
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .map_err(map_function_error)
}

/// UDF カタログの [`CatalogError`] を SQL 表層の契約へ写像する（同名の既存 `42723`・件数上限
/// `54000`・ENUM 等の未対応 `0A000`・形状不正 `22000`・ロック timeout `55P03`・その他 `XX000`）。
/// 文言は固定で、内部キー・tenant_id・本体を含めない（security.md P0）。
fn map_function_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::Invalid(m) if m == FUNCTION_ALREADY_EXISTS_MESSAGE => {
            SqlSurfaceError::duplicate_function("function already exists")
        }
        // 作成時に同名の多重定義を拒否しているため通常は起きない（42725 の扱いは #1917）。
        CatalogError::Invalid(m) if m == FUNCTION_AMBIGUOUS_MESSAGE => SqlSurfaceError::Internal {
            detail: "DROP FUNCTION failed".to_string(),
        },
        CatalogError::Invalid(m) if m.contains("enum") => {
            SqlSurfaceError::feature_not_supported("enum types are not supported in functions")
        }
        CatalogError::Invalid(_) => SqlSurfaceError::invalid_input("invalid function definition"),
        CatalogError::ViewLimitExceeded(_) => {
            SqlSurfaceError::payload_too_large("too many functions")
        }
        // 名前解決後に SCHEMA が削除された（または他テナントの ID）。存在情報を出さない固定文言。
        CatalogError::TableNotFound(_) => {
            SqlSurfaceError::undefined_function("function schema does not exist")
        }
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "function catalog operation failed".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_errors_map_to_fixed_sqlstates() {
        let dup = map_function_error(CatalogError::Invalid(
            FUNCTION_ALREADY_EXISTS_MESSAGE.to_string(),
        ));
        assert_eq!(dup.wire_code(), "42723");
        let limit = map_function_error(CatalogError::ViewLimitExceeded("x".to_string()));
        assert_eq!(limit.wire_code(), "54000");
        let enum_ty = map_function_error(CatalogError::Invalid(
            "enum type is not supported in function signatures".to_string(),
        ));
        assert_eq!(enum_ty.wire_code(), "0A000");
        let other = map_function_error(CatalogError::Invalid("bad".to_string()));
        assert_eq!(other.wire_code(), "22000");
        let corrupt = map_function_error(CatalogError::CorruptSchema("secret #1.f".to_string()));
        assert_eq!(corrupt.wire_code(), "XX000");
        assert!(!corrupt.client_message().contains("secret"));
    }
}
