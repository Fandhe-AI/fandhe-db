//! `INSERT ... SELECT` の内側 SELECT 結果行を `BoundInsert` へ変換する部品（SQL-51・Issue #1733。
//! 親: #1731）。
//!
//! 責務境界: 内側 SELECT の評価（RLS の暗黙適用・行数上限）と書き込みは
//! `core/insert_select_exec.rs` が担い、本モジュールは「結果セル（[`Cell`]）→ 列型の行値
//! （[`Value`]）」と「結果行 → [`BoundInsert`]」の純粋寄りの変換だけを持つ。変換後の
//! `BoundInsert` は複数行 `VALUES`・COPY と同じ書き込み経路（`sql::exec::execute_insert_*_in`）へ
//! 渡る（第 2 の書き込み経路を作らない）。
//!
//! fail-closed 方針: 本実装が代入変換を持たない（セル種別, 列型）の組は黙って丸めず `0A000`
//! （固定文言）で拒否する。構文・型の静的照合（`42804` 等）は構文・束縛側（#1732）の担当で、
//! ここへ到達した不整合は値を含まない固定文言のエラーにする。エラー文言には行の値を含めない
//! （他テナントの存在や内容を観測させないため）。
//!
//! 呼び出し元: `core/insert_select_exec.rs` の `EngineCore::execute_insert_select`。

// 構文・束縛側（#1732）が `ParsedSql` へ結線するまでは crate 内テストからのみ呼ばれる。
#![cfg_attr(not(test), allow(dead_code))]

use super::allowlist::SqlSurfaceError;
use super::exec::{Cell, ResultRow};
use super::parser::{BoundInsert, VolatileColumn};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::numeric::Decimal;
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;

/// 挿入先として解決した 1 列（内側 SELECT の出力列 1 本の行き先）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetColumn {
    /// 疑似列 `id`（従来方式・移行済み表のみ）。
    Id,
    /// `schema.columns` 上の位置。
    Column(usize),
}

/// 挿入先の列リストを解決する。`columns` が `None`（列リスト省略）のときは宣言順で、従来方式の
/// 表では先頭に疑似列 `id` を置く（移行済み表・採番方式の表は `id` を含めない。移行済み表の `id` は
/// 書き込み段が IDENTITY 採番する）。指定がある場合は未知の列を `42703`、重複を `42701` で拒否する。
pub(crate) fn resolve_target_columns(
    schema: &TableSchema,
    columns: Option<&[String]>,
) -> Result<Vec<TargetColumn>, SqlSurfaceError> {
    let has_pseudo_id = schema.has_pseudo_id();
    let mut out: Vec<TargetColumn> = Vec::new();
    match columns {
        None => {
            out.try_reserve_exact(schema.columns.len().saturating_add(1))
                .map_err(|_| internal())?;
            if has_pseudo_id && !schema.is_migrated_id() {
                out.push(TargetColumn::Id);
            }
            out.extend((0..schema.columns.len()).map(TargetColumn::Column));
        }
        Some(names) => {
            out.try_reserve_exact(names.len()).map_err(|_| internal())?;
            for name in names {
                let target = if has_pseudo_id && name == "id" {
                    TargetColumn::Id
                } else {
                    let idx = schema
                        .columns
                        .iter()
                        .position(|c| &c.name == name)
                        .ok_or_else(|| SqlSurfaceError::undefined_column(name.clone()))?;
                    TargetColumn::Column(idx)
                };
                if out.contains(&target) {
                    return Err(SqlSurfaceError::duplicate_column(name.clone()));
                }
                out.push(target);
            }
        }
    }
    if out.is_empty() {
        return Err(SqlSurfaceError::invalid_input(
            "INSERT ... SELECT target column list must not be empty",
        ));
    }
    Ok(out)
}

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

/// 代入変換を持たない（セル種別, 列型）の組の固定エラー（行の値・列名を含めない）。
fn unsupported_pair() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "INSERT ... SELECT does not support this value type conversion",
    )
}

fn out_of_range() -> SqlSurfaceError {
    SqlSurfaceError::numeric_out_of_range("INSERT ... SELECT value is out of range")
}

/// 内側 SELECT のセルを、挿入先の列 `column` の行値へ代入変換する。
///
/// - `NULL`: 非 nullable 列は `23502`（`DEFAULT` は適用しない。明示 NULL と省略は別。TABLE-16）
/// - 文字列系（`VARCHAR(n)`・`CHAR(n)` の長さ検証と空白詰めを含む）・ENUM・JSON は
///   VALUES 形と同じ束縛関数で再検証する
/// - 数値は整数幅の範囲検査（`22003`）・`NUMERIC(p,s)` の再検証・整数 → 浮動小数の拡大を行う
/// - 配列・浮動小数 → 整数などこの Issue が変換を持たない組は `0A000`
pub(crate) fn cell_to_insert_value(
    cell: Cell,
    column: &ColumnDef,
) -> Result<Value, SqlSurfaceError> {
    if matches!(cell, Cell::Null) {
        if !column.nullable {
            return Err(SqlSurfaceError::not_null_violation(column.name.clone()));
        }
        return Ok(Value::Null);
    }
    match (&column.ty, cell) {
        (ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_), Cell::Text(t)) => {
            super::parser::bind_text_literal(&column.ty, &t)
        }
        (ColumnType::Enum(def), Cell::Text(t)) => {
            super::parser::bind_enum_literal(def, &t, &column.name)
        }
        (ColumnType::Json | ColumnType::Jsonb, Cell::Json(j)) => {
            super::parser::bind_json_literal(&j, &column.ty, &column.name)
        }
        (ColumnType::Vector(dim), Cell::Vector(v)) => {
            if u32::try_from(v.len()).ok() != Some(*dim) {
                return Err(SqlSurfaceError::invalid_input(
                    "INSERT ... SELECT vector dimension does not match the column",
                ));
            }
            Ok(Value::Vector(v))
        }
        (
            ColumnType::Numeric { precision, scale },
            c @ (Cell::Numeric(_) | Cell::Integer(_) | Cell::SignedInteger(_)),
        ) => {
            let d = match c {
                Cell::Numeric(d) => d,
                Cell::Integer(u) => {
                    Decimal::from_parts(i128::from(u), 0).map_err(|_| out_of_range())?
                }
                Cell::SignedInteger(i) => {
                    Decimal::from_parts(i128::from(i), 0).map_err(|_| out_of_range())?
                }
                _ => return Err(unsupported_pair()),
            };
            let fitted = super::cast_value::apply_numeric_typmod(d, Some((*precision, *scale)))?;
            Ok(Value::Numeric(fitted))
        }
        // 整数 → 浮動小数の拡大（PostgreSQL の代入変換と同じ向き。逆向きは丸め規則が要るため対象外）。
        (ColumnType::Double, Cell::Integer(u)) => Ok(Value::Double(u as f64)),
        (ColumnType::Double, Cell::SignedInteger(i)) => Ok(Value::Double(i as f64)),
        (ColumnType::Real, Cell::Integer(u)) => Ok(Value::Real(u as f32)),
        (ColumnType::Real, Cell::SignedInteger(i)) => Ok(Value::Real(i as f32)),
        // 配列は要素型の照合規則を持たないため対象外（fail-closed）。
        (ColumnType::Array(_), _) => Err(unsupported_pair()),
        (ty, cell) => match super::matview::cell_to_value(cell, ty) {
            Ok(v) => Ok(v),
            // 同じ型族の組以外は固定文言の `0A000` に写す（matview 用の文言を外へ出さない）。
            Err(SqlSurfaceError::Internal { .. }) => Err(unsupported_pair()),
            // 範囲外も matview 用の文言ではなく本文の固定文言へ写す。
            Err(SqlSurfaceError::NumericOutOfRange { .. }) => Err(out_of_range()),
            Err(e) => Err(e),
        },
    }
}

/// 内側 SELECT の結果行を `BoundInsert` 列へ変換する。省略列は `DEFAULT` を補完し、
/// `DEFAULT nextval` 系の印は束縛直後に `0A000` で拒否する（COPY と同じ fail-closed）。
/// `rows` の行数は呼び出し元が上限検査済みであることを前提に、確保は `try_reserve_exact` で行う。
pub(crate) fn rows_to_bound_inserts(
    table_name: &str,
    schema: &TableSchema,
    targets: &[TargetColumn],
    rows: Vec<ResultRow>,
    operation_id: Option<&OperationId>,
    clock: crate::sql::time_fn::StatementClock,
) -> Result<Vec<BoundInsert>, SqlSurfaceError> {
    let has_pseudo_id = schema.has_pseudo_id();
    let id_targeted = targets.contains(&TargetColumn::Id);
    // 従来方式の表は `id` が必須。移行済み表は省略時に書き込み段が採番する。
    if has_pseudo_id && !id_targeted && !schema.is_migrated_id() {
        return Err(SqlSurfaceError::invalid_input(
            "INSERT ... SELECT column list must include the id pseudo-column",
        ));
    }
    let id_omitted = has_pseudo_id && schema.is_migrated_id() && !id_targeted;

    let mut out: Vec<BoundInsert> = Vec::new();
    out.try_reserve_exact(rows.len()).map_err(|_| internal())?;
    for row in rows {
        if row.cells.len() != targets.len() {
            return Err(SqlSurfaceError::Internal {
                detail: "INSERT ... SELECT result shape changed during the statement".to_string(),
            });
        }
        let mut values: Vec<Value> = vec![Value::Null; schema.columns.len()];
        let mut provided = vec![false; schema.columns.len()];
        let mut id: u64 = 0;
        for (target, cell) in targets.iter().zip(row.cells) {
            match target {
                TargetColumn::Id => id = cell_to_row_id(cell)?,
                TargetColumn::Column(idx) => {
                    let column = schema.columns.get(*idx).ok_or_else(internal)?;
                    let value = cell_to_insert_value(cell, column)?;
                    if let Some(slot) = values.get_mut(*idx) {
                        *slot = value;
                    }
                    if let Some(flag) = provided.get_mut(*idx) {
                        *flag = true;
                    }
                }
            }
        }
        let volatile_default_columns: Vec<VolatileColumn> =
            crate::sql::parser::fill_omitted_columns(
                &schema.columns,
                &mut values,
                &provided,
                Some(clock),
                false,
            )?;
        if volatile_default_columns.iter().any(|c| {
            matches!(
                c,
                VolatileColumn::DefaultSequence(_) | VolatileColumn::Sequence(..)
            )
        }) {
            return Err(crate::sql::parser::reject_sequence_call_literal());
        }
        out.push(BoundInsert {
            table: table_name.to_string(),
            id,
            values,
            operation_id: operation_id.cloned(),
            volatile_default_columns,
            id_omitted,
        });
    }
    Ok(out)
}

/// 疑似列 `id` へ流すセルを `u64` にする（非負整数のみ。NULL・負数は `22000`、他の型は `0A000`）。
fn cell_to_row_id(cell: Cell) -> Result<u64, SqlSurfaceError> {
    match cell {
        Cell::Integer(u) => Ok(u),
        Cell::SignedInteger(i) => u64::try_from(i).map_err(|_| {
            SqlSurfaceError::invalid_input("id pseudo-column value must not be negative")
        }),
        Cell::Null => Err(SqlSurfaceError::invalid_input(
            "id pseudo-column value must not be NULL",
        )),
        _ => Err(unsupported_pair()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: ColumnType, nullable: bool) -> ColumnDef {
        ColumnDef::new(name, ty, nullable)
    }

    // 対象ビヘイビア: SQL-51（ポインタ）。NULL は nullable 列だけが受理する。
    #[test]
    fn null_requires_nullable_column() {
        let nn = col("a", ColumnType::Integer, false);
        let err = cell_to_insert_value(Cell::Null, &nn).unwrap_err();
        assert_eq!(err.wire_code(), "23502");
        let n = col("a", ColumnType::Integer, true);
        assert_eq!(cell_to_insert_value(Cell::Null, &n).unwrap(), Value::Null);
    }

    #[test]
    fn varchar_overflow_is_22001_and_char_is_padded() {
        let v = col("a", ColumnType::Varchar(Some(3)), true);
        let err = cell_to_insert_value(Cell::Text("abcd".into()), &v).unwrap_err();
        assert_eq!(err.wire_code(), "22001");
        let c = col("a", ColumnType::Bpchar(3), true);
        assert_eq!(
            cell_to_insert_value(Cell::Text("ab".into()), &c).unwrap(),
            Value::Text("ab ".into())
        );
    }

    #[test]
    fn integer_widths_check_range() {
        let small = col("a", ColumnType::SmallInt, true);
        let err = cell_to_insert_value(Cell::SignedInteger(70_000), &small).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
        let big = col("a", ColumnType::BigInt, true);
        assert_eq!(
            cell_to_insert_value(Cell::SignedInteger(7), &big).unwrap(),
            Value::BigInt(7)
        );
        let int = col("a", ColumnType::Integer, true);
        assert_eq!(
            cell_to_insert_value(Cell::Integer(7), &int).unwrap(),
            Value::Integer(7)
        );
    }

    #[test]
    fn integer_widens_to_numeric_and_double() {
        let n = col(
            "a",
            ColumnType::Numeric {
                precision: 10,
                scale: 2,
            },
            true,
        );
        let v = cell_to_insert_value(Cell::SignedInteger(5), &n).unwrap();
        assert!(matches!(v, Value::Numeric(d) if d.scale() == 2));
        let overflow = col(
            "a",
            ColumnType::Numeric {
                precision: 3,
                scale: 2,
            },
            true,
        );
        let err = cell_to_insert_value(Cell::SignedInteger(500), &overflow).unwrap_err();
        assert_eq!(err.wire_code(), "22003");
        let d = col("a", ColumnType::Double, true);
        assert_eq!(
            cell_to_insert_value(Cell::SignedInteger(3), &d).unwrap(),
            Value::Double(3.0)
        );
    }

    #[test]
    fn unsupported_pairs_are_0a000_without_values() {
        let int = col("a", ColumnType::Integer, true);
        let err = cell_to_insert_value(Cell::Float(1.5), &int).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
        assert!(!err.to_string().contains("1.5"));
        let t = col("a", ColumnType::Text, true);
        let err = cell_to_insert_value(Cell::Bool(true), &t).unwrap_err();
        assert_eq!(err.wire_code(), "0A000");
    }

    #[test]
    fn vector_dimension_must_match() {
        let v = col("e", ColumnType::Vector(2), false);
        assert!(cell_to_insert_value(Cell::Vector(vec![1.0, 2.0]), &v).is_ok());
        assert!(cell_to_insert_value(Cell::Vector(vec![1.0]), &v).is_err());
    }

    #[test]
    fn target_columns_reject_unknown_and_duplicate() {
        let schema = TableSchema::new(
            "t",
            vec![
                col("e", ColumnType::Vector(2), false),
                col("lang", ColumnType::Text, true),
            ],
        );
        let names = vec!["lang".to_string(), "nope".to_string()];
        assert_eq!(
            resolve_target_columns(&schema, Some(&names))
                .unwrap_err()
                .wire_code(),
            "42703"
        );
        let names = vec!["lang".to_string(), "lang".to_string()];
        assert_eq!(
            resolve_target_columns(&schema, Some(&names))
                .unwrap_err()
                .wire_code(),
            "42701"
        );
        let all = resolve_target_columns(&schema, None).unwrap();
        assert_eq!(
            all,
            vec![
                TargetColumn::Id,
                TargetColumn::Column(0),
                TargetColumn::Column(1)
            ]
        );
    }
}
