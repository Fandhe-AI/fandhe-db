//! `ALTER TABLE ... ALTER COLUMN ... TYPE ... USING <式>` の式の束縛と型検査
//! （TABLE-40 のポインタ、Issue #1635。式の許可範囲は SQL-45、代入変換の規則は SQL-47）。
//!
//! 責務境界: 構文段（`sql::allowlist` の `ALTER COLUMN` 解析）が作る [`Expr`] を、実行段
//! （`sql::ddl::execute_alter_table_alter_column_type`）が DDL 権限ゲート・表の存在確認・
//! 目標型の解決の**後**に本モジュールへ渡す。式は変換前（旧スキーマ）の行を入力として束縛し、
//! 結果の型が新しい型へ代入変換できるかだけを検査する。行は一切読まない。
//!
//! 行の書き換え（代入変換ノードの生成・txn 内での再束縛と世代照合）は後続の Issue #1636、
//! 依存オブジェクトの再構築は Issue #1637 の担当。そのため本関数は型検査だけを行い
//! （`Result<(), _>`）、呼び出し元は成功時に `0A000` で fail-closed する。式を無視して
//! 従来の型変更経路へ流すと意味を取り違える（fail-open）ため。#1636 で束縛結果を返す形へ広げる。
//!
//! エラー文言は固定の英語で、列名と静的な型ラベルだけを含める（値・テナント情報を含めない）。

use crate::catalog::{ColumnType, TableSchema};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::cast::{
    cast_rule, cast_type_assignable, cast_type_of_column, is_assignment_rule, source_cast_kind,
    CastKind, CastType, SourceKind,
};
use crate::sql::udf_call::{
    bind_array_assignment_source, first_non_builtin_call_with, Expr, UdfRegistry, MAX_EXPR_NODES,
};

/// 数値 6 型（種別を静的に決められない数値式の保守的な判定に使う）。
const NUMERIC_KINDS: [CastKind; 6] = [
    CastKind::Int2,
    CastKind::Int4,
    CastKind::Int8,
    CastKind::Float4,
    CastKind::Float8,
    CastKind::Numeric,
];

fn not_supported(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: detail.to_string(),
    }
}

/// USING 式を旧スキーマに対して束縛し、結果が `target` へ代入変換できることを検査する。
/// `column_index` は変換対象列の論理 index（旧スキーマ上）。
///
/// 判定順: 組み込み以外の関数（`42601`）→ VECTOR（`42804`）・ENUM 変換先（`0A000`）→
/// 式の束縛（未定義列 `42703` 等）→ 結果型の代入変換（`42804`／種別不明は `0A000`）。
pub(crate) fn check_alter_using(
    expr: &Expr,
    schema: &TableSchema,
    column_index: usize,
    target: &ColumnType,
) -> Result<(), SqlSurfaceError> {
    // 書き込み経路で UDF・WASM を実行しない。DDL には時刻スコープが無いため現在時刻関数も拒否する。
    if first_non_builtin_call_with(expr, false, true).is_some() {
        return Err(SqlSurfaceError::unsupported(
            "only built-in functions are allowed in a USING expression",
        ));
    }
    let column = schema
        .columns
        .get(column_index)
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "USING target column index is out of range".to_string(),
        })?;
    // VECTOR は次元固定（TABLE-19）のため、変換元・変換先どちらでも型変更を許さない。
    if matches!(column.ty, ColumnType::Vector(_)) || matches!(target, ColumnType::Vector(_)) {
        return Err(SqlSurfaceError::datatype_mismatch(format!(
            "column {:?} cannot be converted with a USING expression to or from a vector type",
            column.name
        )));
    }
    let Some(target_cast) = cast_type_of_column(target) else {
        return Err(not_supported(
            "USING with an enum target type is not supported",
        ));
    };

    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    let (bound, ty, _) = bind_array_assignment_source(expr, schema, &registry, &mut budget)?;

    let convertible = match source_cast_kind(expr, &bound, ty, Some(schema)) {
        SourceKind::Known(k) => is_assignment_rule(cast_rule(k, target_cast.kind())),
        SourceKind::Array(Some(elem)) => {
            cast_type_assignable(&CastType::Array(Box::new(elem)), &target_cast)
        }
        // 種別を静的に決められない数値は、数値 6 型すべてが代入変換できるときだけ受理する。
        SourceKind::UnknownNumeric => NUMERIC_KINDS
            .iter()
            .all(|k| is_assignment_rule(cast_rule(*k, target_cast.kind()))),
        SourceKind::Array(None) | SourceKind::Outside => {
            return Err(not_supported(
                "the result type of this USING expression cannot be determined",
            ))
        }
    };
    if !convertible {
        return Err(SqlSurfaceError::datatype_mismatch(format!(
            "result of USING clause for column {:?} cannot be cast automatically to type {}",
            column.name,
            target_cast.pg_label()
        )));
    }
    Ok(())
}
