//! MATERIALIZED VIEW（以下 matview）の SQL 表層側の補助（TABLE-42・Issue #1664。親: #1662）。
//!
//! 責務境界: カタログと文脈単位の実体ストアは `catalog::materialized`（Issue #1663）が担い、
//! `CREATE MATERIALIZED VIEW` の実行手順は `core.rs`、構文検証は `sql::allowlist` が担う。
//! 本モジュールはその間をつなぐ次の純粋寄りの部品だけを持つ。
//!
//! - 本文の出力列（名前と静的な型）から実体の列定義（`ColumnDef`）を確定する
//!   ([`column_defs_from_view_columns`])。型を静的に持てない列・ENUM 列・識別子として不正な列名・
//!   重複列名は、黙って受理せず拒否する（fail-closed）。
//! - 本文の評価結果のセル（[`Cell`]）を実体の行値（[`Value`]）へ、実体の行値を読み取り結果の
//!   セルへ戻す。列型駆動で、対応しない組み合わせは panic せず `XX000` にする。
//! - `FROM <matview>` の葉の読み取り（[`Statement::MaterializedView`]）の Describe と実行。
//!   読み取りは読み手セッションの `PolicyContext` から導いた文脈キーに完全一致する実体だけを
//!   返し、実体が無ければ `55000`（他文脈の有無・件数は観測できない）。
//!
//! 呼び出し元: `core.rs` の `CreateMaterializedView` 実行腕（列の確定・セル変換）と
//! `execute_read_statement_with_limits`／`describe_parsed_in_session_impl` の
//! `Statement::MaterializedView` 腕（読み取りと Describe）。

use super::allowlist::{SqlSurfaceError, ValidatedMaterializedViewRead};
use super::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use super::view_replace::ViewColumn;
use crate::catalog::{validate_identifier, ColumnDef, ColumnType, MAX_COLUMN_COUNT};
use crate::numeric::Decimal;
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::storage::read_source::ReadSource;

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

/// 本文の出力列を実体の列定義へ確定する。列はすべて nullable で、DEFAULT・照合は持たない。
///
/// 拒否する形（いずれも利用者が別名で回避できる/できない理由を固定文言で伝える）:
/// - 静的な型を持たない列（ベクトル式など）は `0A000`
/// - ENUM 列・ENUM 配列列は `0A000`（実体の decode が語彙解決を持たない。後続課題）
/// - 識別子として不正な列名（`?column?` など）は `42601`（別名の指定を促す）
/// - 最終的な列名の重複は `42701`
/// - 列数が上限を超える場合は `54000`
pub(crate) fn column_defs_from_view_columns(
    columns: &[ViewColumn],
) -> Result<Vec<ColumnDef>, SqlSurfaceError> {
    if columns.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "materialized view has no output columns",
        ));
    }
    if columns.len() > MAX_COLUMN_COUNT {
        return Err(SqlSurfaceError::payload_too_large(
            "materialized view has too many columns",
        ));
    }
    let mut out: Vec<ColumnDef> = Vec::new();
    out.try_reserve_exact(columns.len())
        .map_err(|_| internal())?;
    for col in columns {
        if validate_identifier(&col.name).is_err() {
            return Err(SqlSurfaceError::unsupported(
                "materialized view column needs a valid name; specify a column alias",
            ));
        }
        if out.iter().any(|c| c.name == col.name) {
            return Err(SqlSurfaceError::duplicate_column(col.name.clone()));
        }
        let Some(ty) = &col.ty else {
            return Err(SqlSurfaceError::feature_not_supported(
                "materialized view column has no static type; cast the expression or use a table column",
            ));
        };
        let is_enum = match ty {
            ColumnType::Enum(_) => true,
            ColumnType::Array(a) => a.enum_def().is_some(),
            _ => false,
        };
        if is_enum {
            return Err(SqlSurfaceError::feature_not_supported(
                "ENUM columns are not supported in materialized views",
            ));
        }
        out.push(ColumnDef {
            name: col.name.clone(),
            ty: ty.clone(),
            nullable: true,
            default_expr: None,
            fill_value: None,
            collation: None,
        });
    }
    Ok(out)
}

/// 型不一致の固定エラー（行の値を含めない）。
fn mismatch() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "materialized view value does not match its column type".to_string(),
    }
}

/// 本文の評価結果のセルを、列型 `ty` の実体の行値へ変換する。対応しない組み合わせ・範囲外は
/// 行の値を含まない固定文言で拒否する（範囲外は `22003`、型不一致は `XX000`。panic しない）。
pub(crate) fn cell_to_value(cell: Cell, ty: &ColumnType) -> Result<Value, SqlSurfaceError> {
    if matches!(cell, Cell::Null) {
        return Ok(Value::Null);
    }
    let out_of_range =
        || SqlSurfaceError::numeric_out_of_range("materialized view value is out of range");
    match (ty, cell) {
        (ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_), Cell::Text(t)) => {
            Ok(Value::Text(t))
        }
        (ColumnType::Vector(dim), Cell::Vector(v)) => {
            if u32::try_from(v.len()).ok() != Some(*dim) {
                return Err(mismatch());
            }
            Ok(Value::Vector(v))
        }
        (ColumnType::SmallInt, Cell::SignedInteger(i)) => i16::try_from(i)
            .map(|v| Value::Integer(i32::from(v)))
            .map_err(|_| out_of_range()),
        (ColumnType::SmallInt, Cell::Integer(u)) => i16::try_from(u)
            .map(|v| Value::Integer(i32::from(v)))
            .map_err(|_| out_of_range()),
        (ColumnType::Integer, Cell::SignedInteger(i)) => i32::try_from(i)
            .map(Value::Integer)
            .map_err(|_| out_of_range()),
        (ColumnType::Integer, Cell::Integer(u)) => i32::try_from(u)
            .map(Value::Integer)
            .map_err(|_| out_of_range()),
        (ColumnType::BigInt, Cell::SignedInteger(i)) => Ok(Value::BigInt(i)),
        (ColumnType::BigInt, Cell::Integer(u)) => i64::try_from(u)
            .map(Value::BigInt)
            .map_err(|_| out_of_range()),
        (ColumnType::Real, Cell::Float(f)) => {
            let narrowed = f as f32;
            if f.is_finite() && narrowed.is_finite() {
                Ok(Value::Real(narrowed))
            } else {
                Err(out_of_range())
            }
        }
        (ColumnType::Double, Cell::Float(f)) => {
            if f.is_finite() {
                Ok(Value::Double(f))
            } else {
                Err(out_of_range())
            }
        }
        (ColumnType::Boolean, Cell::Bool(b)) => Ok(Value::Bool(b)),
        (ColumnType::Date, Cell::Date(d)) => Ok(Value::Date(d)),
        (ColumnType::Timestamp, Cell::Timestamp(t)) => Ok(Value::Timestamp(t)),
        (ColumnType::Time, Cell::Time(t)) => Ok(Value::Time(t)),
        (ColumnType::TimestampTz, Cell::TimestampTz(t)) => Ok(Value::TimestampTz(t)),
        (ColumnType::Interval, Cell::Interval(i)) => Ok(Value::Interval(i)),
        (ColumnType::Array(_), Cell::Array(a)) => Ok(Value::Array(a)),
        (ColumnType::Bytea, Cell::Bytes(b)) => Ok(Value::Bytes(b)),
        (ColumnType::Json | ColumnType::Jsonb, Cell::Json(j)) => Ok(Value::Json(j)),
        (ColumnType::Uuid, Cell::Uuid(u)) => Ok(Value::Uuid(u)),
        (ColumnType::Numeric { scale, .. }, Cell::Numeric(d)) => {
            Ok(Value::Numeric(rescale_up(d, *scale)?))
        }
        (ColumnType::Numeric { scale, .. }, Cell::Integer(u)) => {
            let unscaled = i128::from(u);
            Ok(Value::Numeric(rescale_up(
                Decimal::from_parts(unscaled, 0).map_err(|_| out_of_range())?,
                *scale,
            )?))
        }
        (ColumnType::Numeric { scale, .. }, Cell::SignedInteger(i)) => {
            Ok(Value::Numeric(rescale_up(
                Decimal::from_parts(i128::from(i), 0).map_err(|_| out_of_range())?,
                *scale,
            )?))
        }
        _ => Err(mismatch()),
    }
}

/// 位取りを列の `scale` まで増やす（桁を落とす方向の変換は黙って行わず拒否する）。
fn rescale_up(d: Decimal, scale: u8) -> Result<Decimal, SqlSurfaceError> {
    let out_of_range =
        || SqlSurfaceError::numeric_out_of_range("materialized view value is out of range");
    if d.scale() == scale {
        return Ok(d);
    }
    if d.scale() > scale {
        return Err(mismatch());
    }
    let diff = u32::from(scale.saturating_sub(d.scale()));
    let factor = 10i128.checked_pow(diff).ok_or_else(out_of_range)?;
    let unscaled = d.unscaled().checked_mul(factor).ok_or_else(out_of_range)?;
    Decimal::from_parts(unscaled, scale).map_err(|_| out_of_range())
}

/// 実体の行値を、列型 `ty` の読み取り結果のセルへ戻す（[`cell_to_value`] の逆）。
/// 対応しない組み合わせは固定文言の `XX000`（fail-closed）。
pub(crate) fn value_to_cell(value: Value, ty: &ColumnType) -> Result<Cell, SqlSurfaceError> {
    if matches!(value, Value::Null) {
        return Ok(Cell::Null);
    }
    match (ty, value) {
        (ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_), Value::Text(t)) => {
            Ok(Cell::Text(t))
        }
        (ColumnType::Vector(_), Value::Vector(v)) => Ok(Cell::Vector(v)),
        (ColumnType::SmallInt | ColumnType::Integer, Value::Integer(i)) => {
            Ok(Cell::SignedInteger(i64::from(i)))
        }
        (ColumnType::BigInt, Value::BigInt(i)) => Ok(Cell::SignedInteger(i)),
        (ColumnType::Real, Value::Real(f)) => Ok(Cell::Float(f64::from(f))),
        (ColumnType::Double, Value::Double(f)) => Ok(Cell::Float(f)),
        (ColumnType::Boolean, Value::Bool(b)) => Ok(Cell::Bool(b)),
        (ColumnType::Date, Value::Date(d)) => Ok(Cell::Date(d)),
        (ColumnType::Timestamp, Value::Timestamp(t)) => Ok(Cell::Timestamp(t)),
        (ColumnType::Time, Value::Time(t)) => Ok(Cell::Time(t)),
        (ColumnType::TimestampTz, Value::TimestampTz(t)) => Ok(Cell::TimestampTz(t)),
        (ColumnType::Interval, Value::Interval(i)) => Ok(Cell::Interval(i)),
        (ColumnType::Array(_), Value::Array(a)) => Ok(Cell::Array(a)),
        (ColumnType::Bytea, Value::Bytes(b)) => Ok(Cell::Bytes(b)),
        (ColumnType::Json | ColumnType::Jsonb, Value::Json(j)) => Ok(Cell::Json(j)),
        (ColumnType::Uuid, Value::Uuid(u)) => Ok(Cell::Uuid(u)),
        (ColumnType::Numeric { .. }, Value::Numeric(d)) => Ok(Cell::Numeric(d)),
        _ => Err(mismatch()),
    }
}

/// 葉の読み取り（`FROM <matview>`）の結果列メタ。実体の列定義から作るだけで、実体の有無は
/// 判定しない（Describe は本体を実行しない契約）。
pub(crate) fn describe_columns(read: &ValidatedMaterializedViewRead) -> Vec<ColumnMeta> {
    read.columns
        .iter()
        .map(|c| ColumnMeta::Scalar {
            name: c.name.clone(),
            ty: c.ty.clone(),
        })
        .collect()
}

/// 葉の読み取りの実行。読み手の `ctx` に完全一致する文脈の実体だけを `read_txn`（確定済み
/// スナップショットまたは同一トランザクション）から読む。
///
/// - 定義が無い（並行 `DROP`）: `42P01`
/// - 実体が無い（`WITH NO DATA`、または読み手の文脈が実体化対象外）: `55000`。どちらの理由かも、
///   他文脈の実体の有無・件数も区別できない固定応答
/// - 破損（件数不一致・ヘッダ不一致・decode 失敗）: 行を返さず固定文言の `XX000`
pub(crate) fn execute_read<R: ReadSource>(
    read_txn: &R,
    ctx: &PolicyContext,
    read: &ValidatedMaterializedViewRead,
) -> Result<QueryResult, SqlSurfaceError> {
    use crate::catalog::CatalogError;
    let entity = match crate::catalog::read_materialized_view_in(read_txn, &read.name, ctx) {
        Ok(Some(entity)) => entity,
        Ok(None) => return Err(SqlSurfaceError::MaterializedViewNotPopulated),
        Err(CatalogError::ViewNotFound(name)) => {
            return Err(SqlSurfaceError::undefined_table(&name))
        }
        Err(_) => {
            return Err(SqlSurfaceError::Internal {
                detail: "materialized view entity is unreadable".to_string(),
            })
        }
    };
    // 定義の列（読み取り時の再束縛結果）と実体が持つ列が食い違うなら、行を返さず拒否する。
    if entity.columns != read.columns {
        return Err(SqlSurfaceError::Internal {
            detail: "materialized view definition changed during the statement".to_string(),
        });
    }
    let mut rows: Vec<ResultRow> = Vec::new();
    rows.try_reserve_exact(entity.rows.len())
        .map_err(|_| internal())?;
    for values in entity.rows {
        if values.len() != entity.columns.len() {
            return Err(internal());
        }
        let mut cells: Vec<Cell> = Vec::new();
        cells
            .try_reserve_exact(values.len())
            .map_err(|_| internal())?;
        for (value, col) in values.into_iter().zip(entity.columns.iter()) {
            cells.push(value_to_cell(value, &col.ty)?);
        }
        rows.push(ResultRow {
            id: 0,
            score: 0.0,
            cells,
        });
    }
    Ok(QueryResult {
        columns: describe_columns(read),
        rows,
    })
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

    #[test]
    fn column_defs_are_nullable_without_default() {
        let defs = column_defs_from_view_columns(&[
            col("a", Some(ColumnType::Text)),
            col("b", Some(ColumnType::BigInt)),
        ])
        .unwrap();
        assert_eq!(defs.len(), 2);
        assert!(defs.iter().all(|c| c.nullable && c.default_expr.is_none()));
    }

    #[test]
    fn column_defs_reject_untyped_invalid_name_duplicate() {
        let untyped = column_defs_from_view_columns(&[col("a", None)]).unwrap_err();
        assert_eq!(untyped.wire_code(), "0A000");
        let bad_name =
            column_defs_from_view_columns(&[col("?column?", Some(ColumnType::Text))]).unwrap_err();
        assert_eq!(bad_name.wire_code(), "42601");
        let dup = column_defs_from_view_columns(&[
            col("a", Some(ColumnType::Text)),
            col("a", Some(ColumnType::Text)),
        ])
        .unwrap_err();
        assert_eq!(dup.wire_code(), "42701");
    }

    #[test]
    fn cell_value_roundtrip_for_scalar_types() {
        let cases: Vec<(Cell, ColumnType)> = vec![
            (Cell::Text("x".into()), ColumnType::Text),
            (Cell::SignedInteger(7), ColumnType::SmallInt),
            (Cell::SignedInteger(7), ColumnType::Integer),
            (Cell::SignedInteger(-9), ColumnType::BigInt),
            (Cell::Float(1.5), ColumnType::Real),
            (Cell::Float(2.5), ColumnType::Double),
            (Cell::Bool(true), ColumnType::Boolean),
            (Cell::Date(10), ColumnType::Date),
            (Cell::Timestamp(11), ColumnType::Timestamp),
            (Cell::Null, ColumnType::Text),
        ];
        for (cell, ty) in cases {
            let value = cell_to_value(cell.clone(), &ty).unwrap();
            assert_eq!(value_to_cell(value, &ty).unwrap(), cell);
        }
    }

    #[test]
    fn unsigned_count_cells_fit_integer_columns_or_fail_closed() {
        assert_eq!(
            cell_to_value(Cell::Integer(3), &ColumnType::BigInt).unwrap(),
            Value::BigInt(3)
        );
        let huge = cell_to_value(Cell::Integer(u64::MAX), &ColumnType::BigInt).unwrap_err();
        assert_eq!(huge.wire_code(), "22003");
        let id_ty = ColumnType::Numeric {
            precision: 20,
            scale: 0,
        };
        let v = cell_to_value(Cell::Integer(u64::MAX), &id_ty).unwrap();
        assert_eq!(
            value_to_cell(v, &id_ty).unwrap(),
            Cell::Numeric(Decimal::from_parts(i128::from(u64::MAX), 0).unwrap())
        );
    }

    #[test]
    fn smallint_and_vector_dimension_are_checked() {
        let e = cell_to_value(Cell::SignedInteger(40_000), &ColumnType::SmallInt).unwrap_err();
        assert_eq!(e.wire_code(), "22003");
        let e = cell_to_value(Cell::Vector(vec![1.0, 2.0]), &ColumnType::Vector(3)).unwrap_err();
        assert_eq!(e.wire_code(), "XX000");
    }

    #[test]
    fn mismatched_cell_and_type_fail_closed_without_echo() {
        let e = cell_to_value(Cell::Text("secret-value".into()), &ColumnType::BigInt).unwrap_err();
        assert_eq!(e.wire_code(), "XX000");
        assert!(!e.to_string().contains("secret-value"));
        assert!(value_to_cell(Value::Text("x".into()), &ColumnType::Boolean).is_err());
    }

    #[test]
    fn numeric_rescale_up_only() {
        let d = Decimal::from_parts(5, 0).unwrap();
        let up = rescale_up(d, 2).unwrap();
        assert_eq!((up.unscaled(), up.scale()), (500, 2));
        let down = Decimal::from_parts(500, 2).unwrap();
        assert!(rescale_up(down, 0).is_err());
    }
}
