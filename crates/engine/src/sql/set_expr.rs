//! `UPDATE ... SET <col> = <式>` の右辺式の束縛（静的な型照合）と行ごとの評価・列型への
//! 変換（Issue #1544・SQL-45 のポインタ）。
//!
//! 責務境界: `sql::allowlist::Parser` が構文解析した式 [`Expr`]（[`SetValue::Expr`]）を、
//! `sql::parser::bind_set_assignments` が [`bind_set_expr`] で束縛し、`tenant` の
//! read-merge-write 段（`merge_row_for_update_set`）が行ごとに [`eval_set_expr_to_value`] で
//! **更新前の行値**に対して評価して列型の [`Value`] へ変換する。式の束縛・評価そのものは
//! `sql::udf_call` の束縛器・評価器を使い（第 2 の評価器は作らない）、本モジュールは
//! 「対象列の型に代入できるか」の判定と、評価結果から列値への変換だけを担う。
//!
//! fail-closed 方針:
//! - 組み込み以外の関数（セッション UDF・WASM UDF）は `42601`。書き込み経路で WASM を
//!   実行せず、`content_hash` に UDF 閉包を含める必要も生じさせない。
//! - 型が合わない代入は `42804`、VECTOR／NUMERIC 等の列への式は `0A000`。
//! - 評価結果の範囲外・非有限値は `22003`、NULL を NOT NULL 列へ代入する形は `23502`。
//! - 束縛は静的（スキーマのみ参照）で、対象行の探索・RLS 可視性より前に完結する。
//!   値に依存するエラーは `tenant` 側が所有・可視と確定した自テナントの行に対してだけ
//!   発生させる（他テナントの存在オラクルにしない）。

use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::row_codec::{ScalarRef, Value};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::udf_call::{
    bind_projection_expr, eval_with_scalars, first_non_builtin_call_with, BoundExpr, Expr,
    ExprType, ExprValue, UdfRegistry, MAX_EXPR_NODES,
};

/// f64 で整数を正確に表現できる上限（2^53）。これを超える BIGINT への代入は精度欠落の
/// 恐れがあるため `22003` で拒否する（`sql::const_expr` と同じ fail-closed 方針）。
const MAX_EXACT_F64: f64 = 9_007_199_254_740_992.0;

/// 1 日のマイクロ秒数（`DATE` から `TIMESTAMP` への拡大変換用）。
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// 束縛済みの SET 右辺式（Issue #1544）。`bound` は列参照を `ColumnRef { index }` 等で
/// スキーマの論理列 index に解決済みのため、実行時のスキーマが束縛時と同一であること
/// （`tenant` の `expected_schema` 照合）が評価の前提になる。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSetExpr {
    /// 構文木（`content_hash` の入力。評価前の要求由来の情報だけを保持する）。
    pub(crate) source: Expr,
    /// 束縛済みの式。
    pub(crate) bound: BoundExpr,
    /// 代入先の列型。
    pub(crate) target_ty: ColumnType,
    /// 代入先の列が NULL を許すか。
    pub(crate) nullable: bool,
    /// 代入先の列名（`23502` のエラー文言用）。
    pub(crate) column_name: String,
}

impl BoundSetExpr {
    /// 構文木（`content_hash` 計算用）。
    pub(crate) fn source(&self) -> &Expr {
        &self.source
    }
}

/// `expr` を対象列 `column` への代入として束縛する。スキーマだけを参照し、行には触れない。
/// 呼び出し元は `sql::parser::bind_set_assignments`。
pub(crate) fn bind_set_expr(
    name: &str,
    expr: &Expr,
    column: &ColumnDef,
    schema: &TableSchema,
) -> Result<BoundSetExpr, SqlSurfaceError> {
    // 現在時刻関数は、時刻スコープ（原子版 UPDATE の `core.rs` 入口、または分割実行 DML の
    // 固定時刻スコープ）が開いている間に限り SET 右辺で受理する。スコープが無い文脈は
    // 従来どおり 42601。分割実行の `statement_timestamp()`／`clock_timestamp()` の拒否は
    // 束縛段（`udf_call::reject_non_txn_fixed_clock`）が担う（SQL-37・Issue #1587・#2188）。
    let allow_current_time = crate::sql::time_fn::current_clock().is_some();
    if first_non_builtin_call_with(expr, allow_current_time, true).is_some() {
        return Err(SqlSurfaceError::unsupported(
            "only built-in functions are allowed in a SET expression",
        ));
    }
    // 式を受理しない列型。リテラル経路（`bind_set_assignments` の既存分岐）は無変更。
    match &column.ty {
        ColumnType::SmallInt
        | ColumnType::Integer
        | ColumnType::BigInt
        | ColumnType::Real
        | ColumnType::Double
        | ColumnType::Text
        | ColumnType::Varchar(_)
        | ColumnType::Bpchar(_)
        | ColumnType::Boolean
        | ColumnType::Date
        | ColumnType::Timestamp
        | ColumnType::Time => {}
        _ => {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: format!("column {name:?} does not support an expression in SET"),
            })
        }
    }

    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    let (bound, ty, _) = bind_projection_expr(expr, schema, &registry, &mut budget)?;

    let assignable = matches!(
        (&column.ty, ty),
        (
            ColumnType::SmallInt
                | ColumnType::Integer
                | ColumnType::BigInt
                | ColumnType::Real
                | ColumnType::Double,
            ExprType::Scalar
        ) | (
            ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_),
            ExprType::Text
        ) | (ColumnType::Boolean, ExprType::Bool)
            | (ColumnType::Date, ExprType::Date)
            | (ColumnType::Timestamp, ExprType::Timestamp | ExprType::Date)
            | (ColumnType::Time, ExprType::Time)
    );
    if !assignable {
        return Err(SqlSurfaceError::datatype_mismatch(format!(
            "column {name:?} cannot be assigned a value of a different type"
        )));
    }

    Ok(BoundSetExpr {
        source: expr.clone(),
        bound,
        target_ty: column.ty.clone(),
        nullable: column.nullable,
        column_name: name.to_string(),
    })
}

/// 束縛済みの SET 式を 1 行ぶん評価し、対象列型の [`Value`] へ変換する。`row_scalars` は
/// **更新前**の行の論理列スカラービュー（`row_codec::scan_scalar_columns` の結果）、
/// `embedding` は更新前の embedding。呼び出し元は `tenant::merge_row_for_update_set` で、
/// 所有・可視と確定した行に対してだけ呼ぶ。
pub(crate) fn eval_set_expr_to_value<'a>(
    set: &BoundSetExpr,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<Value, SqlSurfaceError> {
    let evaluated = eval_with_scalars(&set.bound, id, embedding, row_scalars)?;
    let internal = || SqlSurfaceError::Internal {
        detail: "SET expression result does not match the target column type".to_string(),
    };
    match evaluated {
        ExprValue::Null => {
            if set.nullable {
                Ok(Value::Null)
            } else {
                Err(SqlSurfaceError::not_null_violation(set.column_name.clone()))
            }
        }
        ExprValue::Scalar(v) => scalar_to_value(&set.target_ty, v).ok_or_else(internal)?,
        ExprValue::Text(t) => match &set.target_ty {
            // `VARCHAR(n)` の n 超過は `22001`（超過部分が半角空白のみなら切り詰め。
            // TABLE-31・Issue #1564）。行ごとの評価結果にしか値が決まらないためここで判定する。
            ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_) => {
                // `CHAR(n)` は空白詰めして保存する（TABLE-31・Issue #1566）。
                crate::sql::parser::bind_text_literal(&set.target_ty, &t)
            }
            _ => Err(internal()),
        },
        ExprValue::Bool(b) => match &set.target_ty {
            ColumnType::Boolean => Ok(Value::Bool(b)),
            _ => Err(internal()),
        },
        // jsonb 値の代入は #1597 まで束縛段が `0A000` で拒否するため到達しない（fail-closed）。
        // json 値（原文保存）の代入も束縛段が `0A000` で拒否するため到達しない（Issue #2180）。
        ExprValue::Json(_) | ExprValue::JsonText(_) => Err(internal()),
        ExprValue::Date(d) => match &set.target_ty {
            ColumnType::Date => Ok(Value::Date(d)),
            ColumnType::Timestamp => i64::from(d)
                .checked_mul(MICROS_PER_DAY)
                .map(Value::Timestamp)
                .ok_or_else(|| SqlSurfaceError::datetime_field_overflow("timestamp out of range")),
            _ => Err(internal()),
        },
        ExprValue::Timestamp(t) => match &set.target_ty {
            ColumnType::Timestamp => Ok(Value::Timestamp(t)),
            _ => Err(internal()),
        },
        // `TIME` は `0..=24:00:00`（両端含む）を再検証し、範囲外は `22008`（Issue #1570）。
        // 行 codec の encode も最後の砦として検査するが、エラー分類をここで確定させる。
        ExprValue::Time(t) => match &set.target_ty {
            ColumnType::Time if (0..=MICROS_PER_DAY).contains(&t) => Ok(Value::Time(t)),
            ColumnType::Time => Err(SqlSurfaceError::datetime_field_overflow(
                "time field value out of range",
            )),
            _ => Err(internal()),
        },
        ExprValue::Vector(_) => Err(internal()),
    }
}

/// スカラー評価結果 `v` を数値列型の [`Value`] へ変換する（数値列以外は `None`）。
/// 整数列への代入は PostgreSQL の float8→int 代入キャストと同じ偶数丸め
/// （`round_ties_even`）。範囲外・非有限値は `22003`。
fn scalar_to_value(ty: &ColumnType, v: f64) -> Option<Result<Value, SqlSurfaceError>> {
    let out_of_range =
        |what: &str| SqlSurfaceError::numeric_out_of_range(format!("{what} out of range"));
    Some(match ty {
        ColumnType::SmallInt => {
            if !v.is_finite() {
                return Some(Err(out_of_range("smallint")));
            }
            let r = v.round_ties_even();
            if r < f64::from(i16::MIN) || r > f64::from(i16::MAX) {
                Err(out_of_range("smallint"))
            } else {
                // 範囲検査済み。値表現は INTEGER と共有（`Value::Integer`）。
                Ok(Value::Integer(i32::from(r as i16)))
            }
        }
        ColumnType::Integer => {
            if !v.is_finite() {
                return Some(Err(out_of_range("integer")));
            }
            let r = v.round_ties_even();
            if r < f64::from(i32::MIN) || r > f64::from(i32::MAX) {
                Err(out_of_range("integer"))
            } else {
                Ok(Value::Integer(r as i32))
            }
        }
        ColumnType::BigInt => {
            if !v.is_finite() {
                return Some(Err(out_of_range("bigint")));
            }
            let r = v.round_ties_even();
            // |r| > 2^53 は f64 で整数を正確に表現できず、丸めた誤値を書き込まないよう拒否する。
            if r.abs() > MAX_EXACT_F64 {
                Err(out_of_range("bigint"))
            } else {
                Ok(Value::BigInt(r as i64))
            }
        }
        ColumnType::Real => {
            let r = v as f32;
            // 非ゼロ値が 0 に丸められる（アンダーフロー）場合も `scalar_float::parse_real` と同じく拒否する。
            if v.is_finite() && r.is_finite() && !(v != 0.0 && r == 0.0) {
                Ok(Value::Real(r))
            } else {
                Err(out_of_range("real"))
            }
        }
        ColumnType::Double => {
            if v.is_finite() {
                Ok(Value::Double(v))
            } else {
                Err(out_of_range("double precision"))
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;

    fn schema() -> TableSchema {
        TableSchema::new(
            "t".to_string(),
            vec![
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("m", ColumnType::Integer, false),
                ColumnDef::new("big", ColumnType::BigInt, true),
                ColumnDef::new("r", ColumnType::Real, true),
                ColumnDef::new("body", ColumnType::Text, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
                ColumnDef::new("d", ColumnType::Date, true),
                ColumnDef::new("ts", ColumnType::Timestamp, true),
            ],
        )
    }

    fn col<'a>(s: &'a TableSchema, name: &str) -> &'a ColumnDef {
        s.columns.iter().find(|c| c.name == name).expect("column")
    }

    fn ident(n: &str) -> Expr {
        Expr::Ident(n.to_string())
    }

    fn num(n: &str) -> Expr {
        Expr::Number(n.to_string())
    }

    fn add(l: Expr, r: Expr) -> Expr {
        Expr::Binary {
            op: crate::sql::udf_call::BinOp::Add,
            lhs: Box::new(l),
            rhs: Box::new(r),
        }
    }

    fn bind(target: &str, expr: &Expr) -> Result<BoundSetExpr, SqlSurfaceError> {
        let s = schema();
        bind_set_expr(target, expr, col(&s, target), &s)
    }

    #[test]
    fn binds_integer_arithmetic_for_integer_column() {
        assert!(bind("n", &add(ident("n"), num("1"))).is_ok());
    }

    #[test]
    fn rejects_type_mismatch_with_42804() {
        let e = bind("body", &add(ident("n"), num("1"))).expect_err("text <- scalar");
        assert_eq!(e.wire_code(), "42804");
        let e = bind("n", &Expr::String("x".to_string())).expect_err("integer <- text");
        assert_eq!(e.wire_code(), "42804");
        let e = bind("flag", &add(ident("n"), num("1"))).expect_err("bool <- scalar");
        assert_eq!(e.wire_code(), "42804");
    }

    #[test]
    fn rejects_non_builtin_call_with_42601() {
        let e = bind(
            "n",
            &Expr::Call {
                name: "my_udf".to_string(),
                args: vec![ident("n")],
            },
        )
        .expect_err("udf");
        assert_eq!(e.wire_code(), "42601");
    }

    #[test]
    fn rejects_expression_for_unsupported_column_type() {
        let s = TableSchema::new(
            "t".to_string(),
            vec![ColumnDef::new("u", ColumnType::Uuid, true)],
        );
        let e = bind_set_expr("u", &num("1"), col(&s, "u"), &s).expect_err("uuid");
        assert_eq!(e.wire_code(), "0A000");
    }

    fn eval(
        target: &str,
        expr: &Expr,
        scalars: &[Option<ScalarRef<'_>>],
    ) -> Result<Value, SqlSurfaceError> {
        let b = bind(target, expr)?;
        eval_set_expr_to_value(&b, 1, &[], scalars)
    }

    fn row(n: i32) -> Vec<Option<ScalarRef<'static>>> {
        let mut v: Vec<Option<ScalarRef<'static>>> = vec![None; 8];
        v[0] = Some(ScalarRef::Integer(n));
        v[1] = Some(ScalarRef::Integer(n));
        v
    }

    #[test]
    fn evaluates_against_the_pre_update_row_value() {
        assert_eq!(
            eval("n", &add(ident("n"), num("1")), &row(41)).unwrap(),
            Value::Integer(42)
        );
    }

    #[test]
    fn integer_overflow_is_22003() {
        let e = eval("n", &add(ident("n"), num("1")), &row(i32::MAX)).expect_err("overflow");
        assert_eq!(e.wire_code(), "22003");
    }

    #[test]
    fn null_into_not_null_column_is_23502() {
        // m は NOT NULL。NULLIF(n, n) は常に NULL。
        let e = eval(
            "m",
            &Expr::NullIf(Box::new(ident("n")), Box::new(ident("n"))),
            &row(1),
        )
        .expect_err("not null");
        assert_eq!(e.wire_code(), "23502");
        // nullable 列なら NULL を書く。
        assert_eq!(
            eval(
                "n",
                &Expr::NullIf(Box::new(ident("n")), Box::new(ident("n"))),
                &row(1),
            )
            .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn real_result_rounds_half_to_even_for_integer_target() {
        let half = Expr::Binary {
            op: crate::sql::udf_call::BinOp::Div,
            lhs: Box::new(ident("n")),
            rhs: Box::new(Expr::Number("2.0".to_string())),
        };
        // 5 / 2.0 = 2.5 -> 偶数丸めで 2
        assert_eq!(eval("n", &half, &row(5)).unwrap(), Value::Integer(2));
        // 7 / 2.0 = 3.5 -> 4
        assert_eq!(eval("n", &half, &row(7)).unwrap(), Value::Integer(4));
    }

    #[test]
    fn real_target_rejects_nonzero_underflow() {
        let r = scalar_to_value(&ColumnType::Real, 1.0e-50).expect("numeric target");
        assert_eq!(r.expect_err("underflow").wire_code(), "22003");
        let r = scalar_to_value(&ColumnType::Real, 0.0).expect("numeric target");
        assert_eq!(r.expect("zero ok"), Value::Real(0.0));
    }

    #[test]
    fn bigint_target_rejects_values_beyond_exact_f64_range() {
        let r = scalar_to_value(&ColumnType::BigInt, 1.0e17).expect("numeric target");
        assert_eq!(r.expect_err("beyond 2^53").wire_code(), "22003");
        let r = scalar_to_value(&ColumnType::Integer, 1.0e12).expect("numeric target");
        assert_eq!(r.expect_err("beyond i32").wire_code(), "22003");
        let r = scalar_to_value(&ColumnType::Integer, 2.5).expect("numeric target");
        assert_eq!(r.expect("ok"), Value::Integer(2));
    }

    #[test]
    fn smallint_target_is_range_checked() {
        let r = scalar_to_value(&ColumnType::SmallInt, 32768.0).expect("numeric target");
        assert_eq!(r.expect_err("beyond i16").wire_code(), "22003");
        let r = scalar_to_value(&ColumnType::SmallInt, -32769.0).expect("numeric target");
        assert_eq!(r.expect_err("below i16").wire_code(), "22003");
        let r = scalar_to_value(&ColumnType::SmallInt, -32768.0).expect("numeric target");
        assert_eq!(r.expect("ok"), Value::Integer(-32768));
        let r = scalar_to_value(&ColumnType::SmallInt, f64::NAN).expect("numeric target");
        assert_eq!(r.expect_err("non-finite").wire_code(), "22003");
    }
}
