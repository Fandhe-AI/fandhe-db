//! `ON CONFLICT ... DO UPDATE SET <col> = <一般の値式>` の束縛（SQL-49・RECOVER-11、
//! Issue #1739）。
//!
//! 役割: `sql::allowlist::Parser` が構文解析した [`UpsertValue::Expr`] を、
//! `sql::parser::bind_upsert_assignments` が [`bind_upsert_expr`] で束縛する。式の束縛・評価
//! そのものは `UPDATE ... SET` の式（`sql::set_expr`、SQL-45）の束縛器・評価器をそのまま使い
//! （第 2 の束縛器・評価器は作らない）、本モジュールは「既存行の列と `EXCLUDED.<列>` を
//! 1 つの行ビューとして式から参照できるようにする」合成スキーマの組み立てだけを担う。
//!
//! 行ビューの形: 論理列 index `0..n` が衝突した既存行（更新前の pre-image）、`n..2n` が
//! `EXCLUDED`（挿入しようとした行）の同じ列順。EXCLUDED 側の列名は構文段が置き換えた
//! 予約接頭辞 [`EXCLUDED_PREFIX`] 付きで、利用者の列名（`[A-Za-z_][A-Za-z0-9_]*`）とは
//! 衝突しない。実行時は `tenant::upsert_typed_rows_unchecked` が同じ並びのスカラービューを
//! 組み立てて [`crate::sql::set_expr::eval_set_expr_to_value`] へ渡す。
//!
//! fail-closed 方針:
//! - 式中の `EXCLUDED.<列>` が `id`（疑似列）・`tenant_id`・`visibility` を指す形は `42601`
//!   （サーバー側が導出・固定する列を新規行の値経由で書き換える迂回路を作らない）。
//! - `EXCLUDED` の VECTOR 列参照は `0A000`、未知列は `42703`（列名のみ。値は含めない）。
//! - 型の食い違い・VECTOR／NUMERIC 等の列への式は `sql::set_expr::bind_set_expr` の分類
//!   （`42804`／`0A000`）に従う。値に依存するエラーは `tenant` 側が所有・可視と確定した
//!   自テナントの衝突行に対してだけ発生させる（他テナントの存在オラクルにしない）。

use std::collections::HashSet;

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::set_expr::{bind_set_expr, BoundSetExpr};
use crate::sql::udf_call::Expr;

/// `EXCLUDED.<列>` を式の列参照へ置き換えるときの識別子の予約接頭辞（NUL 始まり）。
/// 利用者の列名は `catalog::validate_identifier` により NUL を含められず、構文段は
/// 接頭辞で始まる識別子（引用識別子での偽装）を `42601` で拒否する。
pub(crate) const EXCLUDED_PREFIX: &str = "\u{0}excluded.";

/// 束縛済みの一般式（`BoundSetExpr` と、内容照合ハッシュ用の元の構文木を持つ）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundUpsertExpr {
    /// 合成スキーマ上で束縛した式。`source` は構文段が列参照を解決済みの構文木。
    pub(crate) set: BoundSetExpr,
}

impl BoundUpsertExpr {
    /// 評価器へ渡す束縛済み式。
    pub(crate) fn set_expr(&self) -> &BoundSetExpr {
        &self.set
    }
}

/// `expr` を代入先 `column` への `DO UPDATE SET` 右辺として束縛する。
/// `schema` は対象表の（束縛時点の）スキーマ。
pub(crate) fn bind_upsert_expr(
    name: &str,
    expr: &Expr,
    column: &ColumnDef,
    schema: &TableSchema,
) -> Result<BoundUpsertExpr, SqlSurfaceError> {
    let mut idents: HashSet<String> = HashSet::new();
    crate::sql::parser::collect_expr_idents(expr, &mut idents);
    for ident in &idents {
        let Some(src) = ident.strip_prefix(EXCLUDED_PREFIX) else {
            continue;
        };
        if (src == "id" && schema.has_pseudo_id()) || src == "tenant_id" || src == "visibility" {
            return Err(SqlSurfaceError::unsupported(format!(
                "column {src:?} cannot be referenced by EXCLUDED"
            )));
        }
        let src_column = schema
            .columns
            .iter()
            .find(|c| c.name == src)
            .ok_or_else(|| SqlSurfaceError::undefined_column(src))?;
        if matches!(src_column.ty, ColumnType::Vector(_)) {
            return Err(SqlSurfaceError::feature_not_supported(format!(
                "EXCLUDED.{src} of a VECTOR column cannot be used in an expression"
            )));
        }
    }

    let synthetic = synthetic_schema(schema)?;
    let set = bind_set_expr(name, expr, column, &synthetic)?;
    Ok(BoundUpsertExpr { set })
}

/// 既存行（`0..n`）と `EXCLUDED`（`n..2n`）を並べた合成スキーマを作る。
/// 行識別方式は元の表と同じにし、疑似列 `id` の解決を変えない。
fn synthetic_schema(schema: &TableSchema) -> Result<TableSchema, SqlSurfaceError> {
    let n = schema.columns.len();
    let mut columns: Vec<ColumnDef> = Vec::new();
    columns
        .try_reserve_exact(n.saturating_mul(2))
        .map_err(|_| {
            SqlSurfaceError::payload_too_large("failed to reserve ON CONFLICT expression schema")
        })?;
    columns.extend(schema.columns.iter().cloned());
    for c in &schema.columns {
        let mut copy = c.clone();
        copy.name = format!("{EXCLUDED_PREFIX}{}", c.name);
        columns.push(copy);
    }
    Ok(TableSchema::new(schema.name.clone(), columns).with_row_identity(schema.row_identity()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::udf_call::BinOp;

    fn schema() -> TableSchema {
        TableSchema::new(
            "t",
            vec![
                ColumnDef::new("emb", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("label", ColumnType::Text, true),
            ],
        )
    }

    fn ident(s: &str) -> Expr {
        Expr::Ident(s.to_string())
    }

    fn excluded(s: &str) -> Expr {
        Expr::Ident(format!("{EXCLUDED_PREFIX}{s}"))
    }

    fn bin(lhs: Expr, rhs: Expr) -> Expr {
        Expr::Binary {
            op: BinOp::Add,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    fn column<'a>(s: &'a TableSchema, name: &str) -> &'a ColumnDef {
        s.columns.iter().find(|c| c.name == name).expect("column")
    }

    #[test]
    fn binds_existing_and_excluded_columns() {
        let s = schema();
        let expr = bin(ident("n"), excluded("n"));
        let bound = bind_upsert_expr("n", &expr, column(&s, "n"), &s).expect("bind");
        assert_eq!(bound.set_expr().source(), &expr);
    }

    #[test]
    fn rejects_forbidden_excluded_columns() {
        let s = schema();
        for src in ["id", "tenant_id", "visibility"] {
            let expr = bin(ident("n"), excluded(src));
            let err = bind_upsert_expr("n", &expr, column(&s, "n"), &s).unwrap_err();
            assert_eq!(err.wire_code(), "42601", "{src}");
        }
    }

    #[test]
    fn rejects_excluded_vector_and_unknown_columns() {
        let s = schema();
        let err = bind_upsert_expr("n", &excluded("emb"), column(&s, "n"), &s).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
        let err = bind_upsert_expr("n", &excluded("nope"), column(&s, "n"), &s).unwrap_err();
        assert_eq!(err.wire_code(), "42703");
        let err = bind_upsert_expr("n", &ident("nope"), column(&s, "n"), &s).unwrap_err();
        assert_eq!(err.wire_code(), "42703");
    }

    #[test]
    fn rejects_type_mismatch_and_unsupported_target() {
        let s = schema();
        // TEXT 式を INTEGER 列へ
        let err = bind_upsert_expr("n", &excluded("label"), column(&s, "n"), &s).unwrap_err();
        assert_eq!(err.wire_code(), "42804");
        // VECTOR 列への式
        let err = bind_upsert_expr("emb", &ident("n"), column(&s, "emb"), &s).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn synthetic_schema_keeps_logical_order() {
        let s = schema();
        let syn = synthetic_schema(&s).expect("synthetic");
        assert_eq!(syn.columns.len(), s.columns.len() * 2);
        let n = s.columns.len();
        for (i, c) in s.columns.iter().enumerate() {
            assert_eq!(syn.columns[i].name, c.name);
            assert_eq!(
                syn.columns[n + i].name,
                format!("{EXCLUDED_PREFIX}{}", c.name)
            );
            assert_eq!(syn.columns[n + i].ty, c.ty);
        }
        assert_eq!(syn.has_pseudo_id(), s.has_pseudo_id());
    }
}
