//! `CREATE [OR REPLACE] FUNCTION`／`DROP FUNCTION [IF EXISTS]` の永続化経路（SQL-66・TASK-309、
//! Issue #1920・#1917。親: #1918。設計記録: `docs/design/udf-catalog.md`「書き込み経路」）。
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
    FUNCTION_CANNOT_CHANGE_PARAM_NAME_MESSAGE, FUNCTION_CANNOT_CHANGE_RETURN_TYPE_MESSAGE,
    FUNCTION_RECURSIVE_MESSAGE,
};
use crate::catalog::CatalogError;
use crate::sql::allowlist::{DropFunctionBehavior, SqlSurfaceError, ValidatedDropFunction};
use crate::sql::mode::SessionState;
use crate::sql::udf_call::{
    body_reaches_function, commit_function_definition, prepare_function_definition,
    prepare_function_replacement, PreparedUdf,
};
use crate::storage::Storage;

/// `DROP FUNCTION` の成功応答。結果行は持たず、wire はコマンドタグ `DROP FUNCTION` を返す。
///
/// **Issue #1917 で追加した破壊的変更（BREAKING CHANGE）**: `IF EXISTS` で対象が無く何も消さなかった
/// ことを表す `skipped` を追加した（外部から `{}` で構築できなくなる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropFunctionOutcome {
    pub(crate) skipped: bool,
}

impl DropFunctionOutcome {
    /// `IF EXISTS` で対象が無く、何も削除しなかったか。他テナントの定義・存在しない定義はどちらも
    /// `true` になり、存在情報を区別できない（P0）。
    pub fn skipped(&self) -> bool {
        self.skipped
    }
}

/// カタログへ書く [`FunctionDef`] を検証済みの定義から組み立てる（`CREATE`・`OR REPLACE` 共通）。
/// 型注釈つき・RETURNS つきは呼び出し側（`core.rs`）が永続化の前に `0A000` で拒否済みのため、
/// ここでは型なし・戻り値型なしだけを扱う。
fn catalog_def_from_prepared(prepared: &PreparedUdf, body_source: &str) -> FunctionDef {
    FunctionDef {
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
    }
}

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
    let def = catalog_def_from_prepared(&prepared, body_source);
    storage
        .create_function(tenant_id, &def)
        .map_err(map_function_error)?;
    let key = prepared.key.clone();
    commit_function_definition(session.udfs_mut(), prepared);
    // カタログに永続化した定義はカタログ由来として印付けする（他セッションの DROP を refresh が
    // 反映するときの入れ替え対象・権限の無いセッションの DROP の判定に使う。#1921）。
    session.udfs_mut().mark_catalog_origin(&key);
    Ok(())
}

/// `CREATE OR REPLACE FUNCTION` をカタログへ永続化してセッションへ反映する（DDL 権限のあるセッション用。
/// SQL-66 (2)・Issue #1917）。順序は [`prepare_function_replacement`]（レジストリ不変。シグネチャ・引数名・
/// 戻り値型・循環の検査）→ [`Storage::replace_function`]（同名が無ければ作成、あれば同じ内部 ID のまま
/// 置換。単一 write txn・1 回の commit）→ commit 成功後にレジストリを上書きしてカタログ由来の印を付け直す。
/// カタログで失敗したらセッションは変わらない。
pub(crate) fn execute_create_or_replace_function_persistent(
    storage: &Storage,
    tenant_id: &str,
    session: &mut SessionState,
    name: &str,
    params: &[String],
    body: &crate::sql::udf_call::Expr,
    body_source: &str,
) -> Result<(), SqlSurfaceError> {
    let untyped = vec![None; params.len()];
    let prepared =
        prepare_function_replacement(session.udfs(), name, params, &untyped, None, body, true)?;
    let def = catalog_def_from_prepared(&prepared, body_source);
    // 循環検査はセッションのレジストリ（古い可能性がある）ではなく、write txn 内の最新カタログで
    // 再度行う（並行する別セッションの置換との合成で循環が残るのを防ぐ）。
    let target = prepared.key.to_ascii_lowercase();
    let new_body = &prepared.body;
    storage
        .replace_function_checked(tenant_id, &def, &|latest| {
            let mut bodies = std::collections::BTreeMap::new();
            for d in latest {
                let lower = d.function_key.to_ascii_lowercase();
                if lower == target || d.language != FunctionLanguage::Declarative {
                    continue;
                }
                let Ok(tokens) = crate::sql::lexer::tokenize_rendered(&d.body_source) else {
                    continue;
                };
                let Ok(body) = crate::sql::allowlist::parse_stored_function_body(&tokens) else {
                    continue;
                };
                bodies.insert(lower, body);
            }
            if body_reaches_function(new_body, &target, &bodies) {
                return Err(CatalogError::Invalid(
                    FUNCTION_RECURSIVE_MESSAGE.to_string(),
                ));
            }
            Ok(())
        })
        .map_err(map_function_error)?;
    let key = prepared.key.clone();
    commit_function_definition(session.udfs_mut(), prepared);
    session.udfs_mut().mark_catalog_origin(&key);
    Ok(())
}

/// `DROP FUNCTION [IF EXISTS] ... [CASCADE | RESTRICT]` を実行する。DDL 権限のあるセッションは
/// カタログ（自テナントの scope）から削除し、いずれのセッションもレジストリから削除する。
/// **どちらでも消えなかった場合だけ** `42883`（存在情報を出さない固定文言）。`IF EXISTS` なら代わりに
/// `skipped` の成功を返す（他テナントの定義・不在はどちらも同じ応答）。権限の無いセッションはカタログを
/// 見ない。カタログ由来の定義を権限の無いセッションが消そうとすると、`IF EXISTS` でも `42501`。
/// 依存トリガは `RESTRICT`（既定）なら `2BP01`、`CASCADE` なら関数と同じ write txn で連鎖削除する。
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
        // カタログ由来の定義は権限の無いセッションからは消せない（SQL-66・Issue #1921）。呼べる関数に
        // 対して 42883 を返すと事実と食い違うため 42501 にする。セッション内だけの登録は従来どおり消せる。
        if !session.ddl_allowed() && session.udfs().is_catalog_origin(&stmt.name) {
            return Err(SqlSurfaceError::InsufficientPrivilege);
        }
        removed |= session.udfs_mut().remove(&stmt.name);
    }
    if removed {
        Ok(DropFunctionOutcome { skipped: false })
    } else if stmt.if_exists {
        Ok(DropFunctionOutcome { skipped: true })
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
    let cascade = stmt.behavior == DropFunctionBehavior::Cascade;
    let Some(types) = &stmt.arg_types else {
        return storage
            .drop_function_by_name(tenant_id, &stmt.name, cascade)
            .map(|sig| sig.is_some())
            .map_err(map_function_error);
    };
    // 列挙・署名照合・削除を 1 つの write txn で行う（読みと削除の間に別セッションの
    // drop／再作成が割り込んで新しい定義を誤って消さないため）。
    storage
        .drop_function_matching(tenant_id, &stmt.name, cascade, |def| {
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
        // 作成時に同名の多重定義を拒否しているため通常は起きない防御（Issue #1917 で `42725` にした）。
        CatalogError::Invalid(m) if m == FUNCTION_AMBIGUOUS_MESSAGE => {
            SqlSurfaceError::ambiguous_function("function name is not unique")
        }
        // `CREATE OR REPLACE FUNCTION` が引数名・戻り値型を変えようとした（SQL-66 (2)・Issue #1917）。
        CatalogError::Invalid(m) if m == FUNCTION_CANNOT_CHANGE_PARAM_NAME_MESSAGE => {
            SqlSurfaceError::invalid_function_definition("cannot change name of input parameter")
        }
        CatalogError::Invalid(m) if m == FUNCTION_CANNOT_CHANGE_RETURN_TYPE_MESSAGE => {
            SqlSurfaceError::invalid_function_definition(
                "cannot change return type of existing function",
            )
        }
        CatalogError::Invalid(m) if m == FUNCTION_RECURSIVE_MESSAGE => {
            SqlSurfaceError::feature_not_supported(
                "recursive function definitions are not supported",
            )
        }
        CatalogError::Invalid(m) if m.contains("enum") => {
            SqlSurfaceError::feature_not_supported("enum types are not supported in functions")
        }
        CatalogError::Invalid(_) => SqlSurfaceError::invalid_input("invalid function definition"),
        CatalogError::ViewLimitExceeded(_) => {
            SqlSurfaceError::payload_too_large("too many functions")
        }
        // 依存する TRIGGER が残っている（`RESTRICT`。`2BP01`）。内部キー・トリガ名を出さない固定文言。
        CatalogError::DependentObjectsStillExist(_) => {
            SqlSurfaceError::DependentObjectsStillExist {
                name: "function".to_string(),
            }
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

    fn parse_body(src: &str) -> crate::sql::udf_call::Expr {
        let tokens = crate::sql::lexer::tokenize_rendered(src).expect("tokens");
        crate::sql::allowlist::parse_stored_function_body(&tokens).expect("body")
    }

    /// Issue #1917: 古いレジストリで循環検査を通っても、write txn 内の最新カタログで再検査され、
    /// 並行置換 f→g・g→f の合成による循環がカタログに残らない。
    #[test]
    fn replace_rechecks_cycle_against_latest_catalog() {
        let path = crate::test_util::temp_db::unique_db_path("udf-ddl-cycle");
        let _guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open");
        let mut a = SessionState::default();
        let mut b = SessionState::default();
        // 初期状態 f(x) AS x・g(x) AS x を両セッションが同じ状態として持つ。
        for sess in [&mut a, &mut b] {
            let _ = sess;
        }
        let p = vec!["x".to_string()];
        let body_x = parse_body("x");
        let mut seed = SessionState::default();
        let _ = &mut seed;
        let prepared_f = prepare_function_definition(a.udfs(), "f", &p, &body_x).expect("f");
        storage
            .create_function("t", &catalog_def_from_prepared(&prepared_f, "x"))
            .expect("create f");
        commit_function_definition(a.udfs_mut(), prepared_f);
        let prepared_g = prepare_function_definition(a.udfs(), "g", &p, &body_x).expect("g");
        storage
            .create_function("t", &catalog_def_from_prepared(&prepared_g, "x"))
            .expect("create g");
        commit_function_definition(a.udfs_mut(), prepared_g);
        for key in ["f", "g"] {
            let prep = prepare_function_definition(b.udfs(), key, &p, &body_x).expect("b");
            commit_function_definition(b.udfs_mut(), prep);
        }
        execute_create_or_replace_function_persistent(
            &storage,
            "t",
            &mut a,
            "f",
            &p,
            &parse_body("g(x)"),
            "g ( x )",
        )
        .expect("f -> g");
        // b のレジストリは f を単純な本体だと思っているため、レジストリ上の検査は通る。
        let err = execute_create_or_replace_function_persistent(
            &storage,
            "t",
            &mut b,
            "g",
            &p,
            &parse_body("f(x)"),
            "f ( x )",
        )
        .expect_err("g -> f must be rejected by the catalog-level check");
        assert_eq!(err.wire_code(), "0A000");
    }

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
        let ambiguous = map_function_error(CatalogError::Invalid(
            FUNCTION_AMBIGUOUS_MESSAGE.to_string(),
        ));
        assert_eq!(ambiguous.wire_code(), "42725");
        let rename = map_function_error(CatalogError::Invalid(
            FUNCTION_CANNOT_CHANGE_PARAM_NAME_MESSAGE.to_string(),
        ));
        assert_eq!(rename.wire_code(), "42P13");
        let retype = map_function_error(CatalogError::Invalid(
            FUNCTION_CANNOT_CHANGE_RETURN_TYPE_MESSAGE.to_string(),
        ));
        assert_eq!(retype.wire_code(), "42P13");
        let dep = map_function_error(CatalogError::DependentObjectsStillExist(
            "#7.secret".to_string(),
        ));
        assert_eq!(dep.wire_code(), "2BP01");
        assert!(!dep.client_message().contains("secret"));
        let corrupt = map_function_error(CatalogError::CorruptSchema("secret #1.f".to_string()));
        assert_eq!(corrupt.wire_code(), "XX000");
        assert!(!corrupt.client_message().contains("secret"));
    }
}
