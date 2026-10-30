//! PL/pgSQL の変数への代入時の型変換（Issue #2225・SQL-68。設計は `docs/design/plpgsql-subset.md` D4）。
//!
//! `plpgsql::interp` が代入・引数束縛・`RETURN`・`INTO` の各所から呼ぶ。変換の実体は既存の
//! `sql::cast_value::apply_cast`（SQL の `CAST` と同じ入力関数・丸め・範囲検査）に委ね、ここでは
//! `Cell` と式評価値 [`ExprValue`] の往復だけを持つ（第 2 の変換器を作らない）。
//!
//! 対象は数値 6 型（`smallint`／`integer`／`bigint`／`real`／`double precision`／`numeric`）・
//! `boolean`・文字列 3 型。型が同じなら値をそのまま通す。代入規則に無い組は PostgreSQL の
//! PL/pgSQL と同じく「文字列への出力変換 → 入力変換」（I/O 変換）で通し、文字列が不正なら
//! `22P02`／`22003` になる。対象外の型の組は `0A000`（fail-closed。既知の差として設計記録に残す）。
//! エラー文言は SQLSTATE を保ったまま固定の英語に丸め、変数の値を含めない。

use std::borrow::Cow;

use crate::catalog::ColumnType;
use crate::plpgsql::host::PlScalar;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::cast::{cast_type_assignable, cast_type_of_column};
use crate::sql::cast_value::{apply_cast, CastDst, CastFn, CastSrc};
use crate::sql::exec::Cell;
use crate::sql::udf_call::ExprValue;

/// 変換元としての型記述子（対象外の型は `None`）。
fn src_of(ty: &ColumnType) -> Option<CastSrc> {
    Some(match ty {
        ColumnType::SmallInt => CastSrc::Int2,
        ColumnType::Integer => CastSrc::Int4,
        ColumnType::BigInt => CastSrc::Int8,
        ColumnType::Real => CastSrc::Float4,
        ColumnType::Double => CastSrc::Float8,
        ColumnType::Numeric { .. } => CastSrc::Numeric,
        ColumnType::Boolean => CastSrc::Bool,
        ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_) => CastSrc::Text,
        _ => return None,
    })
}

/// 変換先としての型記述子（対象外の型は `None`）。
fn dst_of(ty: &ColumnType) -> Option<CastDst> {
    Some(match ty {
        ColumnType::SmallInt => CastDst::Int2,
        ColumnType::Integer => CastDst::Int4,
        ColumnType::BigInt => CastDst::Int8,
        ColumnType::Real => CastDst::Float4,
        ColumnType::Double => CastDst::Float8,
        ColumnType::Numeric { precision, scale } => CastDst::Numeric(Some((*precision, *scale))),
        ColumnType::Boolean => CastDst::Bool,
        ColumnType::Text => CastDst::Text,
        ColumnType::Varchar(n) => CastDst::Varchar(*n),
        ColumnType::Bpchar(n) => CastDst::Bpchar(*n),
        _ => return None,
    })
}

fn unsupported() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported("unsupported type conversion in PL/pgSQL assignment")
}

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "PL/pgSQL value does not match its declared type".to_string(),
    }
}

/// セルを式評価値へ（`src` の型に合う variant だけ受理する）。
fn to_expr(cell: &Cell, src: CastSrc) -> Result<ExprValue<'static>, SqlSurfaceError> {
    Ok(match (cell, src) {
        (Cell::Null, _) => ExprValue::Null,
        (
            Cell::SignedInteger(i),
            CastSrc::Int2 | CastSrc::Int4 | CastSrc::Int8 | CastSrc::Float4 | CastSrc::Float8,
        ) => ExprValue::Scalar(*i as f64),
        (Cell::Integer(u), CastSrc::Int8) => ExprValue::Scalar(*u as f64),
        (Cell::Float(f), CastSrc::Float4 | CastSrc::Float8) => ExprValue::Scalar(*f),
        (Cell::Numeric(d), CastSrc::Numeric) => ExprValue::Numeric(*d),
        (Cell::Bool(b), CastSrc::Bool) => ExprValue::Bool(*b),
        (Cell::Text(s), CastSrc::Text) => ExprValue::Text(Cow::Owned(s.clone())),
        _ => return Err(internal()),
    })
}

/// 式評価値を変換先の型のセルへ。
fn from_expr(v: ExprValue<'_>, dst: CastDst) -> Result<Cell, SqlSurfaceError> {
    Ok(match (v, dst) {
        (ExprValue::Null, _) => Cell::Null,
        (ExprValue::Scalar(f), CastDst::Int2 | CastDst::Int4 | CastDst::Int8) => {
            Cell::SignedInteger(f as i64)
        }
        (ExprValue::Scalar(f), CastDst::Float4 | CastDst::Float8) => Cell::Float(f),
        (ExprValue::Numeric(d), CastDst::Numeric(_)) => Cell::Numeric(d),
        (ExprValue::Bool(b), CastDst::Bool) => Cell::Bool(b),
        (ExprValue::Text(s), CastDst::Text | CastDst::Varchar(_) | CastDst::Bpchar(_)) => {
            Cell::Text(s.into_owned())
        }
        _ => return Err(internal()),
    })
}

/// 変換器のエラーを SQLSTATE を保ったまま固定文言へ丸める（変数の値を含めない）。
fn sanitize(e: &SqlSurfaceError) -> SqlSurfaceError {
    match e.wire_code() {
        "22P02" => SqlSurfaceError::invalid_text_representation("invalid input syntax for type"),
        "22003" => SqlSurfaceError::numeric_out_of_range("value out of range for type"),
        "22001" => SqlSurfaceError::string_data_right_truncation("value too long for type"),
        "0A000" => unsupported(),
        _ => SqlSurfaceError::invalid_input("invalid value in PL/pgSQL assignment"),
    }
}

/// f64 で桁を失わずに表せる整数の絶対値上限（2^53）。
const F64_EXACT_INT_MAX: u64 = 1 << 53;

/// 整数セルの元の値（`f64` へ落とす前の `i128`）。整数以外は `None`。
fn exact_int(cell: &Cell, from: CastSrc) -> Option<i128> {
    match (cell, from) {
        (Cell::SignedInteger(i), CastSrc::Int2 | CastSrc::Int4 | CastSrc::Int8) => {
            Some(i128::from(*i))
        }
        (Cell::Integer(u), CastSrc::Int8) => Some(i128::from(*u)),
        _ => None,
    }
}

fn convert(cell: &Cell, from: CastSrc, to: CastDst) -> Result<Cell, SqlSurfaceError> {
    // 整数は `f64` 経由で丸まる前に元の値で検査する。2^53 を超える値は、文字列化は
    // 元の整数から直接行い、浮動小数点型への変換は丸めを許し、それ以外は 22003 で拒否する。
    if let Some(i) = exact_int(cell, from) {
        if i.unsigned_abs() > u128::from(F64_EXACT_INT_MAX) {
            return match to {
                CastDst::Text | CastDst::Varchar(_) | CastDst::Bpchar(_) => {
                    let v = ExprValue::Text(Cow::Owned(i.to_string()));
                    let out = apply_cast(
                        CastFn {
                            from: CastSrc::Text,
                            to,
                        },
                        v,
                    )
                    .map_err(|e| sanitize(&e))?;
                    from_expr(out, to)
                }
                CastDst::Float4 | CastDst::Float8 => {
                    // 丸め済みの値を Float8 として渡す（整数用の検査で拒否させない）。
                    let v = ExprValue::Scalar(i as f64);
                    let out = apply_cast(
                        CastFn {
                            from: CastSrc::Float8,
                            to,
                        },
                        v,
                    )
                    .map_err(|e| sanitize(&e))?;
                    from_expr(out, to)
                }
                _ => Err(SqlSurfaceError::numeric_out_of_range(
                    "value out of range for type",
                )),
            };
        }
    }
    let v = to_expr(cell, from)?;
    let out = apply_cast(CastFn { from, to }, v).map_err(|e| sanitize(&e))?;
    from_expr(out, to)
}

/// `v` を宣言型 `to` へ代入変換する。NULL は型だけ付け替える（`NOT NULL` の検査は呼び出し側）。
/// 同じ型は素通し。代入規則に無い組は文字列を経由する I/O 変換、対象外の型の組は `0A000`。
pub(crate) fn assign_cast(v: PlScalar, to: &ColumnType) -> Result<PlScalar, SqlSurfaceError> {
    if v.is_null() {
        return Ok(PlScalar::null(to.clone()));
    }
    if v.ty == *to {
        return Ok(v);
    }
    let (Some(src), Some(dst)) = (src_of(&v.ty), dst_of(to)) else {
        return Err(unsupported());
    };
    // bpchar → text／varchar は末尾の空白を除く（変換規則の文字列型の節）。
    let mut v = v;
    if matches!(v.ty, ColumnType::Bpchar(_))
        && matches!(to, ColumnType::Text | ColumnType::Varchar(_))
    {
        if let Cell::Text(s) = &v.cell {
            v.cell = Cell::Text(s.trim_end_matches(' ').to_string());
        }
    }
    // 代入の変換は長さ超過を 22001 で拒否する（明示 CAST の黙った切り詰めとは別。超過部分が
    // 空白のみなら受理して切り詰める）。文字列以外の値は出力変換した文字列で検査する。
    let limit_of = match dst {
        CastDst::Varchar(n) => n,
        CastDst::Bpchar(n) => Some(n),
        _ => None,
    };
    if let Some(n) = limit_of {
        let text = match &v.cell {
            Cell::Text(s) => Some(s.clone()),
            other => match convert(other, src, CastDst::Text)? {
                Cell::Text(s) => Some(s),
                _ => None,
            },
        };
        if let Some(t) = text {
            let limit = usize::try_from(n).unwrap_or(usize::MAX);
            if t.chars().count() > limit && t.chars().skip(limit).any(|c| c != ' ') {
                return Err(SqlSurfaceError::string_data_right_truncation(
                    "value too long for type",
                ));
            }
        }
    }
    let direct = match (cast_type_of_column(&v.ty), cast_type_of_column(to)) {
        (Some(f), Some(t)) => cast_type_assignable(&f, &t),
        _ => false,
    };
    let cell = if direct {
        convert(&v.cell, src, dst)?
    } else {
        // I/O 変換: 出力変換（文字列化）してから入力変換する。
        let text = convert(&v.cell, src, CastDst::Text)?;
        convert(&text, CastSrc::Text, dst)?
    };
    Ok(PlScalar::new(cell, to.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(i: i64) -> PlScalar {
        PlScalar::new(Cell::SignedInteger(i), ColumnType::Integer)
    }

    fn text(s: &str) -> PlScalar {
        PlScalar::new(Cell::Text(s.to_string()), ColumnType::Text)
    }

    fn code(r: Result<PlScalar, SqlSurfaceError>) -> &'static str {
        r.expect_err("expected an error").wire_code()
    }

    #[test]
    fn same_type_passes_through_and_null_gets_target_type() {
        assert_eq!(assign_cast(int(7), &ColumnType::Integer).unwrap(), int(7));
        let n = assign_cast(PlScalar::null(ColumnType::Text), &ColumnType::Integer).unwrap();
        assert_eq!(n, PlScalar::null(ColumnType::Integer));
    }

    #[test]
    fn integer_to_text_and_bigint() {
        assert_eq!(
            assign_cast(int(42), &ColumnType::Text).unwrap().cell,
            Cell::Text("42".to_string())
        );
        let b = assign_cast(int(42), &ColumnType::BigInt).unwrap();
        assert_eq!(
            b,
            PlScalar::new(Cell::SignedInteger(42), ColumnType::BigInt)
        );
    }

    #[test]
    fn text_to_integer_parses_or_rejects() {
        assert_eq!(
            assign_cast(text(" 12 "), &ColumnType::Integer)
                .unwrap()
                .cell,
            Cell::SignedInteger(12)
        );
        assert_eq!(
            code(assign_cast(text("abc"), &ColumnType::Integer)),
            "22P02"
        );
        assert_eq!(
            code(assign_cast(text("99999999999"), &ColumnType::Integer)),
            "22003"
        );
    }

    #[test]
    fn large_bigint_to_text_is_exact() {
        let b = PlScalar::new(
            Cell::SignedInteger(9_007_199_254_740_993),
            ColumnType::BigInt,
        );
        assert_eq!(
            assign_cast(b, &ColumnType::Text).unwrap().cell,
            Cell::Text("9007199254740993".to_string())
        );
    }

    #[test]
    fn sanitize_keeps_string_truncation_sqlstate() {
        let e = SqlSurfaceError::string_data_right_truncation("x");
        assert_eq!(sanitize(&e).wire_code(), "22001");
    }

    #[test]
    fn varchar_overflow_is_22001_but_trailing_spaces_truncate() {
        let ty = ColumnType::Varchar(Some(3));
        assert_eq!(code(assign_cast(text("abcdef"), &ty)), "22001");
        assert_eq!(code(assign_cast(int(12345), &ty)), "22001");
        let r = assign_cast(text("abc   "), &ty).unwrap();
        assert_eq!(r.cell, Cell::Text("abc".to_string()));
        assert_eq!(
            code(assign_cast(text("abcdef"), &ColumnType::Bpchar(3))),
            "22001"
        );
    }

    #[test]
    fn bpchar_to_text_strips_padding() {
        let c = PlScalar::new(Cell::Text("x  ".to_string()), ColumnType::Bpchar(3));
        let t = assign_cast(c.clone(), &ColumnType::Text).unwrap();
        assert_eq!(t.cell, Cell::Text("x".to_string()));
        let t = assign_cast(c, &ColumnType::Varchar(Some(5))).unwrap();
        assert_eq!(t.cell, Cell::Text("x".to_string()));
    }

    #[test]
    fn narrowing_checks_range() {
        assert_eq!(
            code(assign_cast(int(40_000), &ColumnType::SmallInt)),
            "22003"
        );
        assert_eq!(
            assign_cast(int(100), &ColumnType::SmallInt).unwrap().cell,
            Cell::SignedInteger(100)
        );
    }

    #[test]
    fn large_bigint_to_double_rounds_instead_of_failing() {
        let b = PlScalar::new(
            Cell::SignedInteger(9_007_199_254_740_994),
            ColumnType::BigInt,
        );
        assert_eq!(
            assign_cast(b, &ColumnType::Double).unwrap().cell,
            Cell::Float(9_007_199_254_740_994.0)
        );
    }

    #[test]
    fn double_to_integer_rounds_half_to_even() {
        let d = PlScalar::new(Cell::Float(2.5), ColumnType::Double);
        assert_eq!(
            assign_cast(d, &ColumnType::Integer).unwrap().cell,
            Cell::SignedInteger(2)
        );
        let d = PlScalar::new(Cell::Float(3.5), ColumnType::Double);
        assert_eq!(
            assign_cast(d, &ColumnType::Integer).unwrap().cell,
            Cell::SignedInteger(4)
        );
    }

    #[test]
    fn io_conversion_for_non_assignment_pairs() {
        // boolean -> integer は代入規則に無く、文字列経由では不正入力になる。
        let b = PlScalar::new(Cell::Bool(true), ColumnType::Boolean);
        assert_eq!(code(assign_cast(b, &ColumnType::Integer)), "22P02");
        // text -> boolean は文字列の入力変換で通る。
        let r = assign_cast(text("true"), &ColumnType::Boolean).unwrap();
        assert_eq!(r.cell, Cell::Bool(true));
    }

    #[test]
    fn unsupported_pairs_fail_closed_without_values() {
        let d = PlScalar::new(Cell::Date(1), ColumnType::Date);
        let e = assign_cast(d, &ColumnType::Integer).unwrap_err();
        assert_eq!(e.wire_code(), "0A000");
        let e = assign_cast(text("secret-value"), &ColumnType::Integer).unwrap_err();
        assert!(!e.to_string().contains("secret-value"));
    }
}
