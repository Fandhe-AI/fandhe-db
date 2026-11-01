//! ポリシー式（`CREATE/ALTER POLICY` の `USING (...)`・`WITH CHECK (...)`）の束縛と正規化（RLS-13、
//! Issue #2274。設計記録: `docs/design/row-security-policy-composition.md`）。
//!
//! 役割と責務境界:
//! - `sql::policy_ddl` が `catalog::Storage::create_policy`／`alter_policy` の束縛クロージャの中から
//!   [`bind_policy_expr`] を呼び、同じ write txn で読んだスキーマに対して式を検査する。保存するのは
//!   往復一致を確認した正規化テキストと参照列だけで、利用者が書いた生テキストは保存しない。
//! - 式の評価・合成（行ごとの可視性判定）は本ファイルの責務外（後続 Issue）。本ファイルは「束縛できて、
//!   正規化テキストと往復一致する式」だけを通す。
//!
//! 束縛は `WHERE` と同じ経路（`sql::parser::bind_where_predicates`）を使い、第 2 の束縛器を作らない。
//! 部分索引の述語（`sql::index_predicate`）と違い、決定性検査（`42P17`）は持ち込まない（ポリシーは揮発
//! 関数を禁止しない）。予約列（`tenant_id`・`visibility`）と `visible()` は `42601`、部分クエリ・UDF 等の
//! 未対応の形は `0A000`、真偽値でない式は `42804`、存在しない列は束縛側の `42703` を返す。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::{SqlSurfaceError, WherePredicate};
use crate::sql::index_predicate::collect_column_names;

/// 正規化テキストの長さ上限（バイト。`catalog::policy::MAX_POLICY_EXPR_BYTES` と同値）。
const MAX_POLICY_SQL_LEN: usize = crate::catalog::policy::MAX_POLICY_EXPR_BYTES;
/// 正規化テキストへ往復できない式の拒否文言（固定。値を含めない）。
const MSG_NOT_STORABLE: &str = "policy expression cannot be stored in normalized form";
/// 式が長すぎる場合の拒否文言（固定）。
const MSG_TOO_LONG: &str = "policy expression is too long";
/// 予約列を参照する式の拒否文言。
const MSG_RESERVED: &str = "policy expression must not reference reserved columns";
/// 真偽値でない式の拒否文言（PostgreSQL の文言に合わせる。`42804`）。
const MSG_NOT_BOOLEAN: &str = "argument of policy expression must be type boolean";

/// 束縛・往復一致の確認を終えたポリシー式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundPolicyExpr {
    /// 往復一致を確認済みの正規化 SQL テキスト（カタログへ保存する値）。
    pub(crate) sql: String,
    /// 参照するスキーマ列名（重複なし・スキーマ宣言順。疑似列 `id` は含めない）。
    pub(crate) referenced_columns: Vec<String>,
}

/// ポリシー式を `schema` に対して束縛する。判定順:
/// 1. 予約列（`tenant_id`・`visibility`）の参照は `42601`
/// 2. `WHERE` と同じ束縛（未知列 `42703`・型不整合等は束縛側のエラーを透過）。`visible()` が残れば `42601`
/// 3. 結果が真偽値であること（`42804`）
/// 4. 正規化テキストへ描画（長さ上限 `54000`）し、再解析して元の木と一致することを確認（不一致は `0A000`）
/// 5. 参照列の抽出
///
/// UDF レジストリは空（構文段が組み込み以外の関数を拒否済みで、セッション外で消える関数に依存しない）。
pub(crate) fn bind_policy_expr(
    preds: &[WherePredicate],
    schema: &TableSchema,
) -> Result<BoundPolicyExpr, SqlSurfaceError> {
    let names = collect_column_names(preds)?;
    if names
        .iter()
        .any(|n| n.eq_ignore_ascii_case("tenant_id") || n.eq_ignore_ascii_case("visibility"))
    {
        return Err(SqlSurfaceError::unsupported(MSG_RESERVED));
    }
    check_boolean_result(preds, schema)?;

    let udfs = crate::sql::udf_call::UdfRegistry::default();
    let mut budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let (_metadata, _exprs, rls_present, _or_filters) =
        crate::sql::parser::bind_where_predicates(preds, schema, &udfs, &mut budget, &[])?;
    if rls_present {
        return Err(SqlSurfaceError::unsupported(
            "visible() is not allowed in a policy expression",
        ));
    }

    let sql = crate::sql::check_constraint::render_predicates_with_bool_consts(preds);
    if sql.len() > MAX_POLICY_SQL_LEN {
        return Err(SqlSurfaceError::payload_too_large(MSG_TOO_LONG));
    }
    let not_storable = || SqlSurfaceError::FeatureNotSupported {
        detail: MSG_NOT_STORABLE.to_string(),
    };
    let reparsed =
        crate::sql::allowlist::parse_index_predicate_text(&sql).map_err(|_| not_storable())?;
    if reparsed != preds {
        return Err(not_storable());
    }

    let collected = collect_column_names(preds)?;
    let referenced_columns: Vec<String> = schema
        .columns
        .iter()
        .filter(|c| collected.iter().any(|n| n == &c.name))
        .map(|c| c.name.clone())
        .collect();
    Ok(BoundPolicyExpr {
        sql,
        referenced_columns,
    })
}

/// 保存済みの正規化テキストを untrusted として再解析し、`schema` で再束縛する（`ALTER POLICY` が指定されな
/// かった側の式の参照列を求め直すときに使う唯一の経路）。
pub(crate) fn bind_stored_policy_expr(
    stored_sql: &str,
    schema: &TableSchema,
) -> Result<BoundPolicyExpr, SqlSurfaceError> {
    let preds = crate::sql::allowlist::parse_index_predicate_text(stored_sql)?;
    bind_policy_expr(&preds, schema)
}

/// 2 つの式の参照列を合わせる（重複なし・スキーマ宣言順）。
pub(crate) fn merge_referenced_columns(
    schema: &TableSchema,
    a: &[String],
    b: &[String],
) -> Vec<String> {
    schema
        .columns
        .iter()
        .filter(|c| a.iter().chain(b.iter()).any(|n| n == &c.name))
        .map(|c| c.name.clone())
        .collect()
}

/// 述語の木の裸の列参照（`USING (col)`）が真偽値列かを検査する（`42804`）。比較・`IN`・`LIKE` 等の葉は
/// 構文段が真偽値を返す形だけを通すため対象外。真偽値でない式そのもの（リテラル・算術）は構文段
/// （`sql::allowlist::policy`）が `42804` へ寄せる。存在しない列は束縛側の `42703` に任せる。
fn check_boolean_result(
    preds: &[WherePredicate],
    schema: &TableSchema,
) -> Result<(), SqlSurfaceError> {
    for p in preds {
        match p {
            WherePredicate::BoolColumn { column } => {
                let is_non_boolean = schema
                    .columns
                    .iter()
                    .find(|c| &c.name == column)
                    .is_some_and(|c| !matches!(c.ty, ColumnType::Boolean));
                if is_non_boolean {
                    return Err(SqlSurfaceError::datatype_mismatch(MSG_NOT_BOOLEAN));
                }
            }
            WherePredicate::Not(inner) => {
                check_boolean_result(std::slice::from_ref(inner), schema)?
            }
            WherePredicate::Or(branches) => {
                for b in branches {
                    check_boolean_result(b, schema)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
