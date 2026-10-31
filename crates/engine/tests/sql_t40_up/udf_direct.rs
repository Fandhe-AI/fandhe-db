//! 型付き引数・RETURNS 付き UDF の呼び出し側の挙動を固定する統合テスト用ヘルパー（Issue #1922）。
//!
//! 背景: DDL 権限の無いセッションの `CREATE FUNCTION` は `42501` で拒否され、権限のあるセッションの
//! 型付き引数・RETURNS は永続化が未対応（`0A000`。追跡 Issue は #2798）のため、SQL 経由では
//! これらの関数を作れない。呼び出し側（暗黙変換・型不一致 `42883`・RLS・入れ子呼び出し）の検証は
//! 引き続き必要なので、本ヘルパーは `validate_sql`（構文・型名の検証）と公開 API
//! `define_function_with_signature` でセッションのレジストリへ直接定義する。
//! 権限検査そのものの回帰は `sql66_udf_limits.rs` が担う。呼び出し元は `sql66_udf_param_types.rs`・
//! `sql66_udf_returns.rs` の `run` ヘルパーのみ。

use engine::sql::allowlist::{validate_sql, SqlSurfaceError, Statement, TableLookup};
use engine::sql::mode::SessionState;
use engine::sql::udf_call::define_function_with_signature;
use engine::sql::SqlOutcome;

/// テーブルを持たない照会先（`CREATE FUNCTION` はカタログ照会を要しない）。
struct NoTables;

impl TableLookup for NoTables {
    fn table_exists(&self, _name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(false)
    }
}

/// DDL 権限の無いセッションの（`OR REPLACE` でない）`CREATE FUNCTION` をレジストリへ直接定義する。
/// 対象外の SQL は `None`（呼び出し元が通常の SQL 経路で実行する）。失敗は SQLSTATE 文字列。
pub fn try_define_directly(
    session: &mut SessionState,
    sql: &str,
) -> Option<Result<SqlOutcome, String>> {
    if session.ddl_allowed()
        || !sql
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("CREATE FUNCTION")
    {
        return None;
    }
    define_directly(session, sql)
}

/// 権限の有無にかかわらず `CREATE FUNCTION`（`OR REPLACE` でない）をセッションのレジストリへ直接
/// 定義する（`OR REPLACE` を試験する前提の定義を置くときにも使う）。SQL 経路の検査は通らない。
pub fn define_directly(
    session: &mut SessionState,
    sql: &str,
) -> Option<Result<SqlOutcome, String>> {
    let stmt = match validate_sql(sql, &NoTables) {
        Ok(stmt) => stmt,
        Err(e) => return Some(Err(e.wire_code().to_string())),
    };
    let Statement::CreateFunction {
        name,
        params,
        param_types,
        return_type,
        body,
        ..
    } = stmt
    else {
        return None;
    };
    Some(
        define_function_with_signature(
            session.udfs_mut(),
            &name,
            &params,
            &param_types,
            return_type.as_ref(),
            &body,
        )
        .map(|()| SqlOutcome::CreateFunction { name })
        .map_err(|e| e.wire_code().to_string()),
    )
}
