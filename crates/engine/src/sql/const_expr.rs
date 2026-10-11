//! `INSERT ... VALUES` の要素と列制約 `DEFAULT` で受理する定数式の畳み込み
//! （Issue #1543・SQL-45 のポインタ）。
//!
//! 責務境界: `sql::allowlist::Parser` が構文解析した列参照を含まない式 [`Expr`] を、
//! `sql::udf_call` の束縛器・評価器（第 2 の評価器は作らない）で評価し、既存の
//! [`InsertLiteral`] へ戻す。リテラルへ戻すため、束縛（`sql::parser`）・content_hash・
//! `ColumnDefault`・カタログ形式・wire-server は変更不要。呼び出し元は
//! `allowlist` の VALUES 行解析と列制約 `DEFAULT` 解析。
//!
//! fail-closed 方針: 事前走査は純粋に構文的でカタログを参照しない（DDL 権限ゲートより
//! 前に走っても存在オラクルにならない）。列参照は実在の有無によらず一律 `42703`、
//! 揮発性の値関数・組み込み以外の関数は `42601`、0 除算は `22012`、非有限値・f64 で
//! 正確に表現できない大きさは `22003` で拒否する。

use crate::sql::allowlist::{InsertLiteral, SqlSurfaceError};
use crate::sql::udf_call::{
    bind_const_expr, eval, first_non_builtin_call, BoundExpr, Expr, ExprValue, UdfRegistry,
    MAX_EXPR_NODES,
};

/// f64 が整数を正確に表現できる上限（2^53）。
const MAX_EXACT_F64: f64 = 9_007_199_254_740_992.0;

/// 揮発性・セッション依存の SQL 値関数名（定数畳み込み対象外。`42601`）。
const VALUE_FUNCTION_NAMES: &[&str] = &[
    "CURRENT_TIMESTAMP",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "LOCALTIMESTAMP",
    "LOCALTIME",
    "CURRENT_USER",
    "SESSION_USER",
    "USER",
    "CURRENT_ROLE",
    "CURRENT_SCHEMA",
    "CURRENT_CATALOG",
    "DEFAULT",
];

/// 構文的な事前走査。`Expr` の全 variant を網羅する（`_ =>` を置かない）。
fn prescan(expr: &Expr) -> Result<(), SqlSurfaceError> {
    match expr {
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Null
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_) => Ok(()),
        Expr::Ident(name) => {
            if VALUE_FUNCTION_NAMES
                .iter()
                .any(|v| v.eq_ignore_ascii_case(name))
            {
                Err(SqlSurfaceError::unsupported(
                    "volatile or session-dependent value is not allowed in a constant expression",
                ))
            } else if name.eq_ignore_ascii_case("true") || name.eq_ignore_ascii_case("false") {
                Err(SqlSurfaceError::FeatureNotSupported {
                    detail: "boolean keyword inside an expression is not supported".to_string(),
                })
            } else {
                Err(SqlSurfaceError::UndefinedColumn { name: name.clone() })
            }
        }
        Expr::Call { args, .. } => args.iter().try_for_each(prescan),
        Expr::Binary { lhs, rhs, .. } | Expr::NullIf(lhs, rhs) => {
            prescan(lhs)?;
            prescan(rhs)
        }
        Expr::Case { whens, else_result } => {
            for (c, r) in whens {
                prescan(c)?;
                prescan(r)?;
            }
            match else_result {
                Some(e) => prescan(e),
                None => Ok(()),
            }
        }
        Expr::Coalesce(args) => args.iter().try_for_each(prescan),
    }
}

/// 列参照を含まない式を評価して [`InsertLiteral`] へ畳み込む。
pub(crate) fn fold_const_literal(expr: &Expr) -> Result<InsertLiteral, SqlSurfaceError> {
    prescan(expr)?;
    if first_non_builtin_call(expr).is_some() {
        return Err(SqlSurfaceError::unsupported(
            "only built-in functions are allowed in a constant expression",
        ));
    }
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    let (bound, _ty) = bind_const_expr(expr, &registry, &mut budget)?;
    if crate::sql::udf_call::references_embedding(&bound) || matches!(bound, BoundExpr::IdRef) {
        return Err(SqlSurfaceError::Internal {
            detail: "constant expression resolved to a row reference".to_string(),
        });
    }
    match eval(&bound, 0, &[])? {
        ExprValue::Scalar(v) => {
            if !v.is_finite() || v.abs() > MAX_EXACT_F64 {
                return Err(SqlSurfaceError::numeric_out_of_range(
                    "constant expression result is out of range",
                ));
            }
            if v.fract() == 0.0 {
                // -0.0 は "0" へ正規化する（i64 変換は |v| <= 2^53 を確認済み）。
                Ok(InsertLiteral::Number(format!("{}", v as i64)))
            } else {
                Ok(InsertLiteral::Number(format!("{v}")))
            }
        }
        ExprValue::Text(s) => {
            if s.len() > crate::row_codec::MAX_TEXT_FIELD_LEN as usize {
                return Err(SqlSurfaceError::payload_too_large(
                    "constant expression result exceeds the maximum TEXT field length",
                ));
            }
            Ok(InsertLiteral::String(s.into_owned()))
        }
        ExprValue::Bool(b) => Ok(InsertLiteral::Bool(b)),
        ExprValue::Null => Ok(InsertLiteral::Null),
        ExprValue::Date(d) => Ok(InsertLiteral::String(crate::datetime::format_date(d))),
        ExprValue::Timestamp(t) => Ok(InsertLiteral::String(crate::datetime::format_timestamp(t))),
        ExprValue::Vector(_) => Err(SqlSurfaceError::Internal {
            detail: "constant expression produced a vector".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::udf_call::BinOp;

    fn num(s: &str) -> Expr {
        Expr::Number(s.to_string())
    }
    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary {
            op,
            lhs: Box::new(l),
            rhs: Box::new(r),
        }
    }
    fn call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            name: name.to_string(),
            args,
        }
    }

    #[test]
    fn folds_arithmetic_and_functions() {
        assert_eq!(
            fold_const_literal(&bin(BinOp::Add, num("1"), num("1"))).unwrap(),
            InsertLiteral::Number("2".into())
        );
        assert_eq!(
            fold_const_literal(&call("upper", vec![Expr::String("a".into())])).unwrap(),
            InsertLiteral::String("A".into())
        );
        assert_eq!(
            fold_const_literal(&bin(BinOp::Div, num("1"), num("2"))).unwrap(),
            InsertLiteral::Number("0.5".into())
        );
        assert_eq!(
            fold_const_literal(&Expr::Coalesce(vec![Expr::Null, num("7")])).unwrap(),
            InsertLiteral::Number("7".into())
        );
        assert_eq!(
            fold_const_literal(&Expr::NullIf(Box::new(num("1")), Box::new(num("1")))).unwrap(),
            InsertLiteral::Null
        );
    }

    #[test]
    fn date_roundtrip() {
        let d = crate::datetime::parse_date("2024-01-02").unwrap();
        let InsertLiteral::String(s) = fold_const_literal(&Expr::DateLiteral(d)).unwrap() else {
            panic!("expected string");
        };
        assert_eq!(crate::datetime::parse_date(&s).unwrap(), d);
        let t = crate::datetime::parse_timestamp("2024-01-02 03:04:05").unwrap();
        let InsertLiteral::String(s) = fold_const_literal(&Expr::TimestampLiteral(t)).unwrap()
        else {
            panic!("expected string");
        };
        assert_eq!(crate::datetime::parse_timestamp(&s).unwrap(), t);
    }

    #[test]
    fn rejects_column_refs_with_42703() {
        for e in [
            Expr::Ident("n".into()),
            Expr::Ident("id".into()),
            call("upper", vec![Expr::Ident("n".into())]),
        ] {
            assert!(matches!(
                fold_const_literal(&e),
                Err(SqlSurfaceError::UndefinedColumn { .. })
            ));
        }
    }

    #[test]
    fn rejects_volatile_and_udf_with_42601() {
        for e in [
            Expr::Ident("CURRENT_TIMESTAMP".into()),
            call("now", vec![]),
            call("my_udf", vec![num("1")]),
        ] {
            assert!(
                matches!(
                    fold_const_literal(&e),
                    Err(SqlSurfaceError::UnsupportedSyntax { .. })
                ),
                "{e:?}"
            );
        }
        assert!(matches!(
            fold_const_literal(&Expr::Ident("true".into())),
            Err(SqlSurfaceError::FeatureNotSupported { .. })
        ));
    }

    #[test]
    fn rejects_div_zero_and_range() {
        assert!(matches!(
            fold_const_literal(&bin(BinOp::Div, num("1"), num("0"))),
            Err(SqlSurfaceError::DivisionByZero { .. })
        ));
        assert!(matches!(
            fold_const_literal(&bin(BinOp::Mul, num("9007199254740993"), num("2"))),
            Err(SqlSurfaceError::NumericOutOfRange { .. })
        ));
        assert!(fold_const_literal(&bin(BinOp::Add, Expr::String("a".into()), num("1"))).is_err());
    }
}
