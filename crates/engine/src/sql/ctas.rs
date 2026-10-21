//! `CREATE TABLE ... AS SELECT`（以下 CTAS）の結果列型推論と表定義の組み立て
//! （SQL-52 (1)・(5)、Issue #1677。親: #1676）。
//!
//! 責務境界: 構文受理（#1678）・作成と行投入を 1 つの write txn で行う実行本体（#1678）・
//! 台帳と上限（#1679）は担わない。本モジュールは、SELECT の出力列（Describe の [`ColumnMeta`]）
//! から表の列定義（[`ColumnDef`]）と制約なしの [`ValidatedCreateTable`] を導く純粋関数だけを持つ。
//!
//! 呼び出し元: `core::ctas_exec` が検証済み SELECT を Describe（実行しない）して
//! [`output_columns_from_metas`] → [`derive_ctas_column_defs`] を呼び、実行腕が
//! [`ctas_validated_create_table`] の結果を `sql::ddl::build_table_schema` へ渡す。
//!
//! 判定順序は決定的で、エラー文言は英語の固定文言（表名・テナント・値を含めない。
//! 含めるのはクライアント自身が書いた重複列名だけ）。設計記録は
//! `docs/design/ctas-column-inference.md`。

use super::allowlist::{SqlSurfaceError, ValidatedCreateTable};
use super::exec::{ColumnMeta, ALIASED_ID_TYPE};
use super::view_replace::ViewColumn;
use crate::catalog::{validate_identifier, ColumnDef, ColumnType, MAX_COLUMN_COUNT};

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

/// Describe の出力列を CTAS の入力列へ写す。`id` 疑似列（別名付きを含む）は通常の列として
/// numeric(20,0)（wire の Describe が公告する型）に写し、予約・PK・採番は引き継がない。
pub(crate) fn output_columns_from_metas(metas: Vec<ColumnMeta>) -> Vec<ViewColumn> {
    metas
        .into_iter()
        .map(|m| match m {
            ColumnMeta::Id => ViewColumn {
                name: "id".to_string(),
                ty: Some(ALIASED_ID_TYPE.clone()),
            },
            ColumnMeta::AliasedId { name } => ViewColumn {
                name,
                ty: Some(ALIASED_ID_TYPE.clone()),
            },
            ColumnMeta::Scalar { name, ty } => ViewColumn { name, ty: Some(ty) },
            ColumnMeta::Computed { name, ty } => ViewColumn { name, ty },
        })
        .collect()
}

/// 出力列と任意の列名リストから CTAS の列定義を導く。
///
/// 判定順序: 出力 0 本は `0A000` → 列数がハード上限超過は `54000` → 列名リストが長ければ `42601`
/// → 列ごとに（名前が識別子として不正は `42601`・重複は `42701`・型なしは `0A000`）
/// → VECTOR が 2 本以上なら `42601`。型の修飾（精度・長さ・次元・配列・ENUM）はそのまま保つ。
/// 制約・DEFAULT・照合は持たず、VECTOR 列だけ `nullable = false`（エンジンの不変条件）、他は nullable。
pub(crate) fn derive_ctas_column_defs(
    outputs: &[ViewColumn],
    column_names: &[String],
) -> Result<Vec<ColumnDef>, SqlSurfaceError> {
    if outputs.is_empty() {
        return Err(SqlSurfaceError::feature_not_supported(
            "CREATE TABLE AS query has no output columns",
        ));
    }
    if outputs.len() > MAX_COLUMN_COUNT {
        return Err(SqlSurfaceError::payload_too_large(
            "too many columns in CREATE TABLE AS",
        ));
    }
    if column_names.len() > outputs.len() {
        return Err(SqlSurfaceError::unsupported(
            "CREATE TABLE AS column name list is longer than the query's output columns",
        ));
    }
    let mut out: Vec<ColumnDef> = Vec::new();
    out.try_reserve_exact(outputs.len())
        .map_err(|_| internal())?;
    let mut vector_count: usize = 0;
    for (idx, col) in outputs.iter().enumerate() {
        let name = column_names.get(idx).unwrap_or(&col.name);
        if validate_identifier(name).is_err() {
            return Err(SqlSurfaceError::unsupported(
                "CREATE TABLE AS column needs a valid name; specify a column alias",
            ));
        }
        // RLS 内部列・構文予約語は `CREATE TABLE`／`ADD COLUMN` と同じく拒否する（fail-closed。
        // `id` は CTAS では通常の列として許す契約のため対象外）。ASCII 大文字小文字は無視。
        if is_reserved_ctas_column_name(name) {
            return Err(SqlSurfaceError::unsupported(format!(
                "column name {name:?} is reserved"
            )));
        }
        if out.iter().any(|c| &c.name == name) {
            return Err(SqlSurfaceError::duplicate_column(name.clone()));
        }
        let Some(ty) = &col.ty else {
            return Err(SqlSurfaceError::feature_not_supported(
                "CREATE TABLE AS column has no static type; cast the expression",
            ));
        };
        let is_vector = matches!(ty, ColumnType::Vector(_));
        if is_vector {
            vector_count = vector_count.saturating_add(1);
        }
        out.push(ColumnDef {
            name: name.clone(),
            ty: ty.clone(),
            nullable: !is_vector,
            default_expr: None,
            fill_value: None,
            collation: None,
        });
    }
    if vector_count > 1 {
        return Err(SqlSurfaceError::unsupported(
            "CREATE TABLE AS supports at most one VECTOR column",
        ));
    }
    Ok(out)
}

/// CTAS の列名として使えない予約名か（`tenant_id`／`visibility`／`check`／`constraint`）。
/// `CREATE TABLE`／`ADD COLUMN` の予約列名判定と同じ集合から、CTAS が通常列として許す `id` を除く。
fn is_reserved_ctas_column_name(name: &str) -> bool {
    ["tenant_id", "visibility", "check", "constraint"]
        .iter()
        .any(|r| name.eq_ignore_ascii_case(r))
}

/// 導いた列定義から、制約なし・採番方式（暗黙の `id` を持たない）の [`ValidatedCreateTable`] を作る。
/// `public_qualified` は呼び出し側の名前解決の結果。
pub(crate) fn ctas_validated_create_table(
    table_name: String,
    if_not_exists: bool,
    public_qualified: bool,
    columns: Vec<ColumnDef>,
) -> ValidatedCreateTable {
    ValidatedCreateTable {
        table_name,
        if_not_exists,
        columns,
        alias_type_names: Vec::new(),
        array_len_defaulted: Vec::new(),
        primary_key: None,
        unique_constraints: Vec::new(),
        checks: Vec::new(),
        foreign_keys: Vec::new(),
        primary_key_name: None,
        serial_columns: Vec::new(),
        identity_columns: Vec::new(),
        legacy_row_identity: false,
        public_qualified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: Option<ColumnType>) -> ViewColumn {
        ViewColumn {
            name: name.to_string(),
            ty,
        }
    }
    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn code(r: Result<Vec<ColumnDef>, SqlSurfaceError>) -> &'static str {
        r.expect_err("must fail").wire_code()
    }

    #[test]
    fn sql52_1_names_and_types_pass_through() {
        let outs = [
            col("a", Some(ColumnType::Text)),
            col(
                "b",
                Some(ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                }),
            ),
            col("c", Some(ColumnType::Varchar(Some(8)))),
            col("d", Some(ColumnType::Vector(3))),
        ];
        let defs = derive_ctas_column_defs(&outs, &[]).unwrap();
        assert_eq!(defs.len(), 4);
        assert_eq!(defs[0].name, "a");
        assert_eq!(defs[1].ty, outs[1].ty.clone().unwrap());
        assert_eq!(defs[2].ty, ColumnType::Varchar(Some(8)));
        assert_eq!(defs[3].ty, ColumnType::Vector(3));
        assert!(defs[0].nullable && defs[1].nullable && defs[2].nullable);
        assert!(!defs[3].nullable);
        assert!(defs
            .iter()
            .all(|d| d.default_expr.is_none() && d.collation.is_none() && d.fill_value.is_none()));
    }

    #[test]
    fn sql52_1_column_name_list_overrides_and_fills_rest() {
        let outs = [
            col("a", Some(ColumnType::Text)),
            col("b", Some(ColumnType::Integer)),
        ];
        let defs = derive_ctas_column_defs(&outs, &names(&["x"])).unwrap();
        assert_eq!(defs[0].name, "x");
        assert_eq!(defs[1].name, "b");
        let defs = derive_ctas_column_defs(&outs, &names(&["x", "y"])).unwrap();
        assert_eq!(defs[1].name, "y");
    }

    #[test]
    fn sql52_1_name_list_longer_is_42601() {
        let outs = [col("a", Some(ColumnType::Text))];
        assert_eq!(
            code(derive_ctas_column_defs(&outs, &names(&["x", "y"]))),
            "42601"
        );
    }

    #[test]
    fn sql52_1_duplicate_names_are_42701_and_list_can_resolve() {
        let outs = [
            col("a", Some(ColumnType::Text)),
            col("a", Some(ColumnType::Text)),
        ];
        assert_eq!(code(derive_ctas_column_defs(&outs, &[])), "42701");
        assert!(derive_ctas_column_defs(&outs, &names(&["a", "b"])).is_ok());
        assert_eq!(
            code(derive_ctas_column_defs(&outs, &names(&["z", "z"]))),
            "42701"
        );
    }

    #[test]
    fn sql52_1_unnamed_expression_is_42601_unless_listed() {
        let outs = [col("?column?", Some(ColumnType::Integer))];
        assert_eq!(code(derive_ctas_column_defs(&outs, &[])), "42601");
        assert!(derive_ctas_column_defs(&outs, &names(&["n"])).is_ok());
    }

    #[test]
    fn sql52_1_untyped_column_is_0a000() {
        let outs = [col("a", None)];
        assert_eq!(code(derive_ctas_column_defs(&outs, &[])), "0A000");
    }

    #[test]
    fn sql52_1_empty_output_is_0a000() {
        assert_eq!(code(derive_ctas_column_defs(&[], &[])), "0A000");
    }

    #[test]
    fn sql52_1_too_many_columns_is_54000() {
        let outs: Vec<ViewColumn> = (0..=MAX_COLUMN_COUNT)
            .map(|i| col(&format!("c{i}"), Some(ColumnType::Integer)))
            .collect();
        assert_eq!(code(derive_ctas_column_defs(&outs, &[])), "54000");
    }

    #[test]
    fn sql52_1_two_vector_columns_is_42601() {
        let outs = [
            col("a", Some(ColumnType::Vector(2))),
            col("b", Some(ColumnType::Vector(2))),
        ];
        assert_eq!(code(derive_ctas_column_defs(&outs, &[])), "42601");
    }

    #[test]
    fn sql52_1_error_order_is_deterministic() {
        // 一覧が長い（42601）は、重複（42701）・型なし（0A000）より先に判定する。
        let outs = [col("a", None), col("a", None)];
        assert_eq!(
            code(derive_ctas_column_defs(&outs, &names(&["p", "q", "r"]))),
            "42601"
        );
        // 同じ列では重複（42701）が型なし（0A000）より先。
        let outs = [col("a", Some(ColumnType::Text)), col("a", None)];
        assert_eq!(code(derive_ctas_column_defs(&outs, &[])), "42701");
        // 列エラー（0A000）は VECTOR 本数（42601）より先。
        let outs = [
            col("a", Some(ColumnType::Vector(2))),
            col("b", Some(ColumnType::Vector(2))),
            col("c", None),
        ];
        assert_eq!(code(derive_ctas_column_defs(&outs, &[])), "0A000");
    }

    #[test]
    fn sql52_1_id_pseudo_columns_become_plain_numeric_columns() {
        let outs = output_columns_from_metas(vec![
            ColumnMeta::Id,
            ColumnMeta::AliasedId {
                name: "k".to_string(),
            },
        ]);
        let defs = derive_ctas_column_defs(&outs, &[]).unwrap();
        assert_eq!(defs[0].name, "id");
        assert_eq!(defs[1].name, "k");
        assert!(defs.iter().all(|d| d.ty == ALIASED_ID_TYPE && d.nullable));
    }

    #[test]
    fn sql52_1_reserved_column_names_rejected_via_list_and_alias() {
        for r in ["tenant_id", "Visibility", "CHECK", "constraint"] {
            let outs = [col("a", Some(ColumnType::Text))];
            assert_eq!(code(derive_ctas_column_defs(&outs, &names(&[r]))), "42601");
            let outs = [col(r, Some(ColumnType::Text))];
            assert_eq!(code(derive_ctas_column_defs(&outs, &[])), "42601");
        }
    }

    #[test]
    fn sql52_5_validated_create_table_has_no_constraints_and_allocated_identity() {
        let defs = derive_ctas_column_defs(&[col("a", Some(ColumnType::Text))], &[]).unwrap();
        let v = ctas_validated_create_table("t2".to_string(), true, false, defs);
        assert!(v.if_not_exists && !v.legacy_row_identity && v.primary_key.is_none());
        assert!(
            v.unique_constraints.is_empty() && v.checks.is_empty() && v.foreign_keys.is_empty()
        );
        assert!(v.serial_columns.is_empty() && v.identity_columns.is_empty());
    }
}
