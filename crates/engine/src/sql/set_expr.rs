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
    /// `SET col = DEFAULT`（SQL-50・Issue #1746）の束縛結果。`Some` のとき本項は式ではなく
    /// 束縛時に解決済みの既定値（未定義なら NULL）で、評価は行を読まずこの値を返す。
    /// 式項の搬送路（束縛時スキーマとの `expected_schema` 照合・分割実行・内容照合ハッシュの
    /// 構文形印）を共有するために同じ型で運ぶ。`source` は使われない占位。
    pub(crate) default_value: Option<Value>,
}

impl BoundSetExpr {
    /// 構文木（`content_hash` 計算用）。
    pub(crate) fn source(&self) -> &Expr {
        &self.source
    }

    /// `id` 指定形 UPDATE の内容照合ハッシュ用の値側（`tenant` が呼ぶ）。`DEFAULT` 項は
    /// 解決値ではなく構文形の印（`Default`）を返す（TABLE-39 (6)・RECOVER-10 のポインタ:
    /// 内容照合は送られた文で行い、列 DEFAULT の変更後の再送でも同一ハッシュにする）。
    pub(crate) fn hash_item(&self) -> crate::recovery::content_hash::UpdateColumnHashItem<'_> {
        if self.default_value.is_some() {
            crate::recovery::content_hash::UpdateColumnHashItem::Default
        } else {
            crate::recovery::content_hash::UpdateColumnHashItem::Expr(&self.source)
        }
    }
}

/// `SET col = DEFAULT`（SQL-50・Issue #1746）を列 `column` への代入として束縛する。スキーマ
/// （列定義）だけを参照し行には触れない。列 DEFAULT があればその定数値、無ければ NULL
/// （`SET col = NULL` と同じ規則: NOT NULL 列は `23502`、VECTOR 列は `22000`）。
/// 時刻関数・SEQUENCE・IDENTITY の DEFAULT は評価文脈が無いため `bind_column_default` の
/// `0A000` をそのまま返す（fail-closed。GENERATED ALWAYS 列もこの拒否になる）。
pub(crate) fn bind_set_default(
    name: &str,
    column: &ColumnDef,
) -> Result<BoundSetExpr, SqlSurfaceError> {
    let value = match &column.default_expr {
        Some(default) => crate::sql::parser::bind_column_default(column, default)?,
        None => match &column.ty {
            ColumnType::Vector(_) => {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "column {name:?} is a VECTOR column and cannot be set to NULL"
                )))
            }
            _ if column.nullable => Value::Null,
            _ => return Err(SqlSurfaceError::not_null_violation(name)),
        },
    };
    Ok(BoundSetExpr {
        source: Expr::Ident("DEFAULT".to_string()),
        bound: BoundExpr::Number(0.0),
        target_ty: column.ty.clone(),
        nullable: column.nullable,
        column_name: name.to_string(),
        default_value: Some(value),
    })
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
        | ColumnType::TimestampTz
        | ColumnType::Interval
        | ColumnType::Time
        // 配列列（TABLE-35・Issue #1602）。右辺は配列式（コンストラクタ・スライス・CAST・配列列）。
        | ColumnType::Array(_) => {}
        _ => {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: format!("column {name:?} does not support an expression in SET"),
            })
        }
    }

    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    // 配列列への代入だけ、式の最上位が配列値であることを許す（他の位置の cast-only 型の拒否規則は
    // 変えない）。配列以外の型が来たら下の `assignable` が `42804` にする。
    let (bound, ty, _) = if matches!(column.ty, ColumnType::Array(_)) {
        crate::sql::udf_call::bind_array_assignment_source(expr, schema, &registry, &mut budget)?
    } else {
        bind_projection_expr(expr, schema, &registry, &mut budget)?
    };

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
            | (ColumnType::Interval, ExprType::Interval)
            // `TIMESTAMPTZ` 列への代入（Issue #1576）。TIMESTAMP・DATE はセッション TZ の壁時計として、
            // TIMESTAMP・DATE 列への TIMESTAMPTZ は局所の値として変換する（PG の代入キャスト）。
            | (
                ColumnType::TimestampTz,
                ExprType::TimestampTz | ExprType::Timestamp | ExprType::Date
            )
            | (ColumnType::Timestamp | ColumnType::Date, ExprType::TimestampTz)
            | (ColumnType::Array(_), ExprType::Array)
    );
    if !assignable {
        return Err(SqlSurfaceError::datatype_mismatch(format!(
            "column {name:?} cannot be assigned a value of a different type"
        )));
    }

    let bound = match &column.ty {
        ColumnType::Array(at) => crate::sql::cast::bind_array_assignment(expr, bound, schema, at)?,
        _ => convert_for_assignment(&column.ty, ty, bound),
    };
    Ok(BoundSetExpr {
        source: expr.clone(),
        bound,
        target_ty: column.ty.clone(),
        nullable: column.nullable,
        column_name: name.to_string(),
        default_value: None,
    })
}

/// 日時型どうしの代入時の変換ノードを挟む（セッション TZ は評価時に読む。Issue #1576）。
/// 変換不要な組（同型）はそのまま返す。
fn convert_for_assignment(
    target: &ColumnType,
    from: ExprType,
    bound: crate::sql::udf_call::BoundExpr,
) -> crate::sql::udf_call::BoundExpr {
    use crate::sql::udf_call::{BoundExpr, BuiltinFn};
    let wrap = |f: BuiltinFn, inner: BoundExpr| BoundExpr::Builtin {
        f,
        args: vec![inner],
    };
    match (target, from) {
        (ColumnType::TimestampTz, ExprType::Timestamp) => wrap(BuiltinFn::TimestampToTstz, bound),
        (ColumnType::TimestampTz, ExprType::Date) => wrap(BuiltinFn::DateToTstz, bound),
        (ColumnType::Timestamp, ExprType::TimestampTz) => wrap(BuiltinFn::TstzToTimestamp, bound),
        (ColumnType::Date, ExprType::TimestampTz) => wrap(BuiltinFn::TstzToDate, bound),
        _ => bound,
    }
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
    if let Some(value) = &set.default_value {
        return Ok(value.clone());
    }
    let evaluated = eval_with_scalars(&set.bound, id, embedding, row_scalars)?;
    expr_value_to_column_value(&set.target_ty, set.nullable, &set.column_name, evaluated)
}

/// [`eval_set_expr_to_value`] の NOT NULL 検査後回し版。評価結果が NULL なら列の NULL 可否に関わらず
/// `Value::Null` を返す。BEFORE UPDATE トリガ経路（Issue #1725）が NEW 候補を値の列として作り、
/// BEFORE の連鎖後の最終行にだけ NOT NULL 検査を行うために使う（BEFORE が NULL を補正・Skip できる）。
pub(crate) fn eval_set_expr_to_value_deferred_not_null<'a>(
    set: &BoundSetExpr,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<Value, SqlSurfaceError> {
    if let Some(value) = &set.default_value {
        return Ok(value.clone());
    }
    let evaluated = eval_with_scalars(&set.bound, id, embedding, row_scalars)?;
    expr_value_to_column_value(&set.target_ty, true, &set.column_name, evaluated)
}

/// 式の評価結果 `evaluated` を列型 `target_ty` の [`Value`] へ変換する（列値化）。
/// `UPDATE ... SET` の [`eval_set_expr_to_value`] と、`ALTER COLUMN TYPE` の全行変換
/// （`column_rewrite`。TABLE-40・Issue #1636）が共有する単一の変換器で、第 2 の変換器は
/// 作らない。`nullable` が偽の列への NULL は `23502`（`column_name` はスキーマ由来）。
/// 型の食い違いは `Internal`（束縛段の不変条件が崩れた場合の保険。fail-closed）。
pub(crate) fn expr_value_to_column_value(
    target_ty: &ColumnType,
    nullable: bool,
    column_name: &str,
    evaluated: ExprValue<'_>,
) -> Result<Value, SqlSurfaceError> {
    let internal = || SqlSurfaceError::Internal {
        detail: "expression result does not match the target column type".to_string(),
    };
    match evaluated {
        ExprValue::Null => {
            if nullable {
                Ok(Value::Null)
            } else {
                Err(SqlSurfaceError::not_null_violation(column_name))
            }
        }
        ExprValue::Scalar(v) => scalar_to_value(target_ty, v).ok_or_else(internal)?,
        ExprValue::Text(t) => match target_ty {
            // `VARCHAR(n)` の n 超過は `22001`（超過部分が半角空白のみなら切り詰め。
            // TABLE-31・Issue #1564）。行ごとの評価結果にしか値が決まらないためここで判定する。
            ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_) => {
                // `CHAR(n)` は空白詰めして保存する（TABLE-31・Issue #1566）。
                crate::sql::parser::bind_text_literal(target_ty, &t)
            }
            _ => Err(internal()),
        },
        ExprValue::Bool(b) => match target_ty {
            ColumnType::Boolean => Ok(Value::Bool(b)),
            _ => Err(internal()),
        },
        // `UPDATE ... SET` では jsonb・json・numeric・uuid・bytea の代入を束縛段（`bind_set_expr`）が
        // `0A000` で拒否するためこれらの腕には到達しない。`ALTER COLUMN TYPE` の全行変換
        // （Issue #1636）だけが、代入変換ノードの結果としてここへ運ぶ。
        // jsonb の列には正規化済みのテキスト、json の列には原文テキストだけを入れる。
        ExprValue::Json(t) => match target_ty {
            ColumnType::Jsonb => Ok(Value::Json(t.into_owned())),
            _ => Err(internal()),
        },
        ExprValue::JsonText(t) => match target_ty {
            ColumnType::Json => Ok(Value::Json(t.into_owned())),
            _ => Err(internal()),
        },
        // 変換ノードの型修飾子適用で列の位取りへ正規化済みであることを再検証する
        // （行 codec は位取りを持たず、不一致を黙って受理すると値の意味が変わる）。
        ExprValue::Numeric(d) => match target_ty {
            ColumnType::Numeric { precision, scale }
                if d.scale() == *scale && d.fits_precision(*precision) =>
            {
                Ok(Value::Numeric(d))
            }
            ColumnType::Numeric { .. } => Err(SqlSurfaceError::numeric_out_of_range(
                "numeric field overflow",
            )),
            _ => Err(internal()),
        },
        ExprValue::Uuid(u) => match target_ty {
            ColumnType::Uuid => Ok(Value::Uuid(u)),
            _ => Err(internal()),
        },
        ExprValue::Bytes(b) => match target_ty {
            ColumnType::Bytea => Ok(Value::Bytes(b.into_owned())),
            _ => Err(internal()),
        },
        // 配列値（TABLE-35・Issue #1602）。束縛段が要素型を列に合わせ済みで、ここでは要素型の一致と
        // 要素数・フレーム長の上限を行ごとの評価結果に対して再検証する（`54000`。encode と同じ上限）。
        ExprValue::Array(a) => match target_ty {
            ColumnType::Array(at) => {
                if !crate::row_codec::array_elem_kind_eq(a.elem(), at.elem()) {
                    return Err(internal());
                }
                if a.len() as u64 > u64::from(at.max_len()) {
                    return Err(SqlSurfaceError::payload_too_large(
                        "array element count exceeds the column limit",
                    ));
                }
                crate::row_codec::scalar_array_entry_len(at.elem(), &a).map_err(|_| {
                    SqlSurfaceError::payload_too_large("array value exceeds the maximum size")
                })?;
                Ok(Value::Array(a))
            }
            _ => Err(internal()),
        },
        ExprValue::Date(d) => match target_ty {
            ColumnType::Date => Ok(Value::Date(d)),
            ColumnType::Timestamp => i64::from(d)
                .checked_mul(MICROS_PER_DAY)
                .map(Value::Timestamp)
                .ok_or_else(|| SqlSurfaceError::datetime_field_overflow("timestamp out of range")),
            _ => Err(internal()),
        },
        ExprValue::Timestamp(t) => match target_ty {
            ColumnType::Timestamp => Ok(Value::Timestamp(t)),
            _ => Err(internal()),
        },
        // `TIME` は `0..=24:00:00`（両端含む）を再検証し、範囲外は `22008`（Issue #1570）。
        // 行 codec の encode も最後の砦として検査するが、エラー分類をここで確定させる。
        ExprValue::Time(t) => match target_ty {
            ColumnType::Time if (0..=MICROS_PER_DAY).contains(&t) => Ok(Value::Time(t)),
            ColumnType::Time => Err(SqlSurfaceError::datetime_field_overflow(
                "time field value out of range",
            )),
            _ => Err(internal()),
        },
        // `INTERVAL` は無限大の予約値（番兵）と一致しないことを再検証し、一致したら `22008`
        // （Issue #1581。行 codec の encode も最後の砦として検査する）。
        ExprValue::Interval(v) => match target_ty {
            ColumnType::Interval if crate::datetime::validate_interval(&v) => {
                Ok(Value::Interval(v))
            }
            ColumnType::Interval => Err(SqlSurfaceError::datetime_field_overflow(
                "interval out of range",
            )),
            _ => Err(internal()),
        },
        // `TIMESTAMPTZ` は UTC の表現範囲を再検証し、範囲外は `22008`（Issue #1576）。
        ExprValue::TimestampTz(t) => match target_ty {
            ColumnType::TimestampTz if crate::datetime::validate_timestamptz_micros(t) => {
                Ok(Value::TimestampTz(t))
            }
            ColumnType::TimestampTz => Err(SqlSurfaceError::datetime_field_overflow(
                "timestamptz out of range",
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
