//! `ON CONFLICT ... DO UPDATE SET <col> = <col> + (<数値リテラル> | EXCLUDED.<col>)` の
//! 厳密評価（SQL-49・RECOVER-11、Issue #2425）。
//!
//! 役割: `sql::parser::bind_upsert_assignments`（束縛。リテラルの型付け・型組み合わせ判定）と
//! `tenant::upsert_typed_rows_unchecked`（衝突した可視な既存行の pre-image に対する
//! read-merge-write）から呼ばれる。`sql::set_expr` は f64 を経由し 2^53 超の BIGINT を
//! 厳密に扱えないため使わず、整数は i128 の checked 演算、浮動小数は f64 で計算する。
//! 範囲外は `22003`、NOT NULL 列への NULL は `23502`。エラー文言は列名・型名のみで値を含めない。

use crate::catalog::ColumnType;
use crate::row_codec::Value;
use crate::sql::allowlist::{InsertLiteral, SqlSurfaceError};

/// 整数の列型（SMALLINT・INTEGER・BIGINT）か。
pub(crate) fn is_integer_type(ty: &ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::SmallInt | ColumnType::Integer | ColumnType::BigInt
    )
}

/// 浮動小数の列型（REAL・DOUBLE PRECISION）か。
pub(crate) fn is_float_type(ty: &ColumnType) -> bool {
    matches!(ty, ColumnType::Real | ColumnType::Double)
}

/// 代入先と `EXCLUDED.<src>` の列型の組み合わせを判定する（束縛段）。
/// NUMERIC は `0A000`、数値以外は `42883`、整数の代入先への浮動小数は `0A000`。
pub(crate) fn check_operand_pair(
    target: &ColumnType,
    operand: &ColumnType,
) -> Result<(), SqlSurfaceError> {
    check_target_type(target)?;
    check_target_type(operand)?;
    if is_integer_type(target) && is_float_type(operand) {
        return Err(SqlSurfaceError::feature_not_supported(
            "addition of a floating-point operand to an integer column is not supported",
        ));
    }
    Ok(())
}

/// 代入先（または `EXCLUDED` 側）の列型が加算の対象か判定する。
pub(crate) fn check_target_type(ty: &ColumnType) -> Result<(), SqlSurfaceError> {
    if is_integer_type(ty) || is_float_type(ty) {
        return Ok(());
    }
    if matches!(ty, ColumnType::Numeric { .. }) {
        return Err(SqlSurfaceError::feature_not_supported(
            "addition on NUMERIC columns in ON CONFLICT SET is not supported",
        ));
    }
    Err(SqlSurfaceError::undefined_function(
        "operator does not exist: + for the column type",
    ))
}

/// 数値リテラルを代入先の列型に合わせた `Value`（`BigInt`／`Double`）へ束縛する。
/// この値はハッシュ材料（送られた文の形）にもそのまま使う。
pub(crate) fn bind_add_literal(
    target: &ColumnType,
    literal: &InsertLiteral,
) -> Result<Value, SqlSurfaceError> {
    let InsertLiteral::Number(raw) = literal else {
        return Err(SqlSurfaceError::unsupported(
            "ON CONFLICT SET addition requires a numeric literal",
        ));
    };
    let text = raw.strip_prefix('+').unwrap_or(raw);
    if is_integer_type(target) {
        let digits = text.strip_prefix('-').unwrap_or(text);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(SqlSurfaceError::feature_not_supported(
                "non-integer literal added to an integer column is not supported",
            ));
        }
        return text
            .parse::<i64>()
            .map(Value::BigInt)
            .map_err(|_| SqlSurfaceError::numeric_out_of_range("bigint out of range"));
    }
    if is_float_type(target) {
        return match text.parse::<f64>() {
            // 非ゼロ仮数のリテラルが 0.0 に丸まる（アンダーフロー）場合は拒否する。
            // 黙って 0 加算になると範囲検査をすり抜け、台帳ハッシュも衝突するため。
            Ok(f) if f.is_finite() && !(f == 0.0 && has_nonzero_mantissa(text)) => {
                Ok(Value::Double(f))
            }
            _ => Err(SqlSurfaceError::numeric_out_of_range(
                "double precision out of range",
            )),
        };
    }
    check_target_type(target).map(|()| Value::Null)
}

/// 指数部を除いた仮数に 0 以外の数字が含まれるか（アンダーフロー判定用）。
fn has_nonzero_mantissa(text: &str) -> bool {
    text.split(['e', 'E'])
        .next()
        .is_some_and(|m| m.bytes().any(|b| (b'1'..=b'9').contains(&b)))
}

fn int_value(v: &Value) -> Option<i128> {
    match v {
        Value::Integer(i) => Some(i128::from(*i)),
        Value::BigInt(i) => Some(i128::from(*i)),
        _ => None,
    }
}

fn float_value(v: &Value) -> Option<f64> {
    match v {
        Value::Real(f) => Some(f64::from(*f)),
        Value::Double(f) => Some(*f),
        _ => int_value(v).map(|i| i as f64),
    }
}

/// `existing + operand` を代入先の列型で厳密に評価する（pre-image と束縛済みオペランド）。
/// どちらかが NULL なら NULL（NOT NULL 列は `23502`）。範囲外は `22003`。
/// 束縛済みの不変条件に反する `Value` 種別は `Internal`（黙って NULL にしない）。
pub(crate) fn eval_upsert_add(
    target: &ColumnType,
    nullable: bool,
    column_name: &str,
    existing: &Value,
    operand: &Value,
) -> Result<Value, SqlSurfaceError> {
    if matches!(existing, Value::Null) || matches!(operand, Value::Null) {
        return if nullable {
            Ok(Value::Null)
        } else {
            Err(SqlSurfaceError::not_null_violation(column_name.to_string()))
        };
    }
    let internal = || SqlSurfaceError::Internal {
        detail: "upsert addition operand type mismatch".to_string(),
    };
    if is_integer_type(target) {
        let a = int_value(existing).ok_or_else(internal)?;
        let b = int_value(operand).ok_or_else(internal)?;
        let sum = a
            .checked_add(b)
            .ok_or_else(|| SqlSurfaceError::numeric_out_of_range("bigint out of range"))?;
        return match target {
            ColumnType::SmallInt => i16::try_from(sum)
                .map(|v| Value::Integer(i32::from(v)))
                .map_err(|_| SqlSurfaceError::numeric_out_of_range("smallint out of range")),
            ColumnType::Integer => i32::try_from(sum)
                .map(Value::Integer)
                .map_err(|_| SqlSurfaceError::numeric_out_of_range("integer out of range")),
            _ => i64::try_from(sum)
                .map(Value::BigInt)
                .map_err(|_| SqlSurfaceError::numeric_out_of_range("bigint out of range")),
        };
    }
    if is_float_type(target) {
        let a = float_value(existing).ok_or_else(internal)?;
        let b = float_value(operand).ok_or_else(internal)?;
        let sum = a + b;
        let inputs_non_finite = !a.is_finite() || !b.is_finite();
        if matches!(target, ColumnType::Real) {
            // f64 で加算してから f32 へ丸める。f32 同士の和は f64 で正確に表現できるため、
            // f32 で直接加算した結果と一致する（二重丸めは起きない）。
            let narrowed = sum as f32;
            if !inputs_non_finite && (!narrowed.is_finite() || (sum != 0.0 && narrowed == 0.0)) {
                return Err(SqlSurfaceError::numeric_out_of_range("real out of range"));
            }
            return Ok(Value::Real(narrowed));
        }
        if !inputs_non_finite && !sum.is_finite() {
            return Err(SqlSurfaceError::numeric_out_of_range(
                "double precision out of range",
            ));
        }
        return Ok(Value::Double(sum));
    }
    Err(internal())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> InsertLiteral {
        InsertLiteral::Number(s.to_string())
    }

    fn code(e: SqlSurfaceError) -> &'static str {
        e.wire_code()
    }

    fn add(ty: &ColumnType, nullable: bool, a: Value, b: Value) -> Result<Value, SqlSurfaceError> {
        eval_upsert_add(ty, nullable, "n", &a, &b)
    }

    #[test]
    fn integer_boundaries_are_22003() {
        let r = add(
            &ColumnType::Integer,
            true,
            Value::Integer(i32::MAX),
            Value::BigInt(1),
        );
        assert_eq!(code(r.unwrap_err()), "22003");
        let r = add(
            &ColumnType::BigInt,
            true,
            Value::BigInt(i64::MIN),
            Value::BigInt(-1),
        );
        assert_eq!(code(r.unwrap_err()), "22003");
        let r = add(
            &ColumnType::SmallInt,
            true,
            Value::Integer(32767),
            Value::BigInt(1),
        );
        assert_eq!(code(r.unwrap_err()), "22003");
        let r = add(
            &ColumnType::SmallInt,
            true,
            Value::Integer(-32768),
            Value::BigInt(-1),
        );
        assert_eq!(code(r.unwrap_err()), "22003");
    }

    #[test]
    fn bigint_beyond_2_pow_53_is_exact() {
        let base = (1_i64 << 53) + 1;
        let r = add(
            &ColumnType::BigInt,
            true,
            Value::BigInt(base),
            Value::BigInt(1),
        )
        .unwrap();
        assert_eq!(r, Value::BigInt(base + 1));
    }

    #[test]
    fn narrower_target_accepts_wider_operand_when_result_fits() {
        let r = add(
            &ColumnType::Integer,
            true,
            Value::Integer(5),
            Value::BigInt(-3_000_000_000),
        );
        assert_eq!(code(r.unwrap_err()), "22003");
        let r = add(
            &ColumnType::Integer,
            true,
            Value::Integer(i32::MAX),
            Value::BigInt(-10),
        )
        .unwrap();
        assert_eq!(r, Value::Integer(i32::MAX - 10));
    }

    #[test]
    fn null_propagates_or_violates_not_null() {
        let r = add(&ColumnType::Integer, true, Value::Null, Value::BigInt(1));
        assert_eq!(r.unwrap(), Value::Null);
        let r = add(&ColumnType::Integer, false, Value::Integer(1), Value::Null);
        assert_eq!(code(r.unwrap_err()), "23502");
    }

    #[test]
    fn real_overflow_and_double_inf() {
        let r = add(
            &ColumnType::Real,
            true,
            Value::Real(f32::MAX),
            Value::Double(f64::from(f32::MAX)),
        );
        assert_eq!(code(r.unwrap_err()), "22003");
        let r = add(
            &ColumnType::Real,
            true,
            Value::Real(1.5),
            Value::Double(0.25),
        )
        .unwrap();
        assert_eq!(r, Value::Real(1.75));
        let r = add(
            &ColumnType::Double,
            true,
            Value::Double(f64::MAX),
            Value::Double(f64::MAX),
        );
        assert_eq!(code(r.unwrap_err()), "22003");
        let r = add(
            &ColumnType::Double,
            true,
            Value::Double(f64::INFINITY),
            Value::Double(1.0),
        )
        .unwrap();
        assert_eq!(r, Value::Double(f64::INFINITY));
    }

    #[test]
    fn literal_binding_rules() {
        assert_eq!(
            bind_add_literal(&ColumnType::Integer, &lit("-3")).unwrap(),
            Value::BigInt(-3)
        );
        assert_eq!(
            bind_add_literal(&ColumnType::Integer, &lit("+3")).unwrap(),
            Value::BigInt(3)
        );
        assert_eq!(
            code(bind_add_literal(&ColumnType::Integer, &lit("1.5")).unwrap_err()),
            "0A000"
        );
        assert_eq!(
            code(bind_add_literal(&ColumnType::Integer, &lit("1e2")).unwrap_err()),
            "0A000"
        );
        assert_eq!(
            code(bind_add_literal(&ColumnType::Integer, &lit("99999999999999999999")).unwrap_err()),
            "22003"
        );
        assert_eq!(
            bind_add_literal(&ColumnType::Double, &lit("0.5")).unwrap(),
            Value::Double(0.5)
        );
        for t in ["1e-999", "-1e-999", "0.5e-400"] {
            assert_eq!(
                code(bind_add_literal(&ColumnType::Double, &lit(t)).unwrap_err()),
                "22003"
            );
        }
        assert_eq!(
            bind_add_literal(&ColumnType::Double, &lit("0.0e-999")).unwrap(),
            Value::Double(0.0)
        );
    }

    #[test]
    fn type_pair_classification() {
        assert!(check_operand_pair(&ColumnType::Integer, &ColumnType::BigInt).is_ok());
        assert!(check_operand_pair(&ColumnType::Double, &ColumnType::Integer).is_ok());
        assert_eq!(
            code(check_operand_pair(&ColumnType::Integer, &ColumnType::Double).unwrap_err()),
            "0A000"
        );
        assert_eq!(
            code(check_operand_pair(&ColumnType::Text, &ColumnType::Integer).unwrap_err()),
            "42883"
        );
        let numeric = ColumnType::Numeric {
            precision: 10,
            scale: 2,
        };
        assert_eq!(
            code(check_operand_pair(&numeric, &ColumnType::Integer).unwrap_err()),
            "0A000"
        );
    }
}
