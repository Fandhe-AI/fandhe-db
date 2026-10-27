//! 式の内側へ埋め込む相関スカラーサブクエリの値の葉（SQL-61 ポインタ、Issue #1848、親 #1845。
//! RLS-10 ポインタ）。
//!
//! 責務境界: `sql::subquery` が内側を 1 回だけ実行して作った値プローブ
//! （[`crate::sql::join::semi::CorrelatedProbe`]。集計形は相関キー → 集計値の表、非集計形は
//! 内側の可視行の実体化）を、束縛済みの式木の葉（`BoundExpr::CorrelatedScalar`）として
//! 外側の算術・比較・`COALESCE`・`CASE` などへ載せる。評価は外側の行ごとに [`CorrelatedScalarRef::eval`] が
//! プローブを引いて値を返すだけで、第 2 の式評価器は作らない（再帰評価 `udf_call::eval` と
//! コンパイル済み `expr_program` の両方が同じ入口を呼ぶ）。
//!
//! 値の意味は #1846／#1847 の投影位置と同じ。該当なしは NULL（集計形は空集合に対する集計値で、
//! `count` は 0）、2 件以上は `21000`、再評価の上限超過は `54000`。いずれも葉を評価した時点
//! （外側の行を評価した時点）に 1 か所で起きるため、外側が 0 行なら発生しない（遅延評価の規約）。
//! 内側の束縛後の実行時データ例外は遅延エラーとして保持し、同じく評価時点で返す。
//!
//! 可視行由来の値を持つため `Debug` は不透明にする（エラー・ログ経由の漏えい防止）。

use std::borrow::Cow;
use std::sync::Arc;

use crate::catalog::ColumnType;
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::{Cell, ColumnMeta};
use crate::sql::join::semi::CorrelatedProbeRef;
use crate::sql::udf_call::{ExprType, ExprValue};

/// `f64` で正確に表現できる整数の上限（`2^53`。`udf_call::id_as_finite_scalar` と同じ境界）。
const MAX_EXACT_F64_INT: u64 = 1u64 << 53;

/// 葉の値源。
enum ScalarSource {
    /// 内側を実行済みの値プローブ（集計形・非集計形のどちらも `scalar_value` で引く）。
    Probe(CorrelatedProbeRef),
    /// 内側の束縛後の実行時データ例外（`22xxx`・入れ子の `21000`）。外側の行を評価したときだけ返す。
    Deferred(SqlSurfaceError),
}

/// 葉の中身。
struct Leaf {
    source: ScalarSource,
    /// 内側の結果列の静的な型（式の型・結果列型の根拠）。
    ty: ColumnType,
}

/// 式の内側の相関スカラーの葉への共有ハンドル（`BoundExpr::CorrelatedScalar` が保持する）。
///
/// `PartialEq`／`Eq` は同一の葉か（`Arc::ptr_eq`）で判定し、`Debug` は値を出さない。
#[derive(Clone)]
pub struct CorrelatedScalarRef(Arc<Leaf>);

impl CorrelatedScalarRef {
    /// 実行済みの値プローブから葉を作る。
    pub(crate) fn from_probe(probe: CorrelatedProbeRef, ty: ColumnType) -> Self {
        Self(Arc::new(Leaf {
            source: ScalarSource::Probe(probe),
            ty,
        }))
    }

    /// 遅延する実行時エラーを持つ葉を作る（外側の行を評価した時点でそのエラーを返す）。
    pub(crate) fn deferred(error: SqlSurfaceError, ty: ColumnType) -> Self {
        Self(Arc::new(Leaf {
            source: ScalarSource::Deferred(error),
            ty,
        }))
    }

    /// 内側の結果列の静的な型。
    pub(crate) fn column_type(&self) -> &ColumnType {
        &self.0.ty
    }

    /// 式の型（算術・比較・`COALESCE` が参照する）。構築側が [`expr_type_for`] で検証済みの型だけを持たせる。
    pub(crate) fn expr_type(&self) -> Result<ExprType, SqlSurfaceError> {
        expr_type_for(&self.0.ty)
    }

    /// デコード対象へ含める外側の列（相関キー・残余が参照する列）。遅延エラーの葉は持たない。
    pub(crate) fn outer_columns(&self) -> &[usize] {
        match &self.0.source {
            ScalarSource::Probe(p) => p.probe().outer_columns(),
            ScalarSource::Deferred(_) => &[],
        }
    }

    /// 外側の 1 行について値を返す。該当なしは NULL、2 件以上は `21000`、遅延エラーはそのエラー。
    pub(crate) fn eval(
        &self,
        row_scalars: &[Option<ScalarRef<'_>>],
        id: u64,
    ) -> Result<ExprValue<'static>, SqlSurfaceError> {
        match &self.0.source {
            ScalarSource::Deferred(e) => Err(e.clone()),
            ScalarSource::Probe(p) => match p.probe().scalar_value(row_scalars, id)? {
                None => Ok(ExprValue::Null),
                Some(cell) => cell_to_value(cell, &self.0.ty),
            },
        }
    }
}

impl PartialEq for CorrelatedScalarRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CorrelatedScalarRef {}

impl std::fmt::Debug for CorrelatedScalarRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CorrelatedScalarRef(..)")
    }
}

/// 内側の結果列の列型を式の型へ写す。値として運べない型（VECTOR・JSON 系・配列・`CHAR(n)`）は
/// `0A000`（静的な文言。列名・値を含めない）。
pub(crate) fn expr_type_for(ty: &ColumnType) -> Result<ExprType, SqlSurfaceError> {
    Ok(match ty {
        ColumnType::SmallInt
        | ColumnType::Integer
        | ColumnType::BigInt
        | ColumnType::Real
        | ColumnType::Double => ExprType::Scalar,
        ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Enum(_) => ExprType::Text,
        ColumnType::Boolean => ExprType::Bool,
        ColumnType::Date => ExprType::Date,
        ColumnType::Timestamp => ExprType::Timestamp,
        ColumnType::Time => ExprType::Time,
        ColumnType::TimestampTz => ExprType::TimestampTz,
        ColumnType::Interval => ExprType::Interval,
        ColumnType::Numeric { .. } => ExprType::Numeric,
        ColumnType::Uuid => ExprType::Uuid,
        ColumnType::Bytea => ExprType::Bytea,
        ColumnType::Bpchar(_)
        | ColumnType::Vector(_)
        | ColumnType::Array(_)
        | ColumnType::Json
        | ColumnType::Jsonb => {
            return Err(SqlSurfaceError::feature_not_supported(
                "this column type is not supported in a scalar subquery inside an expression",
            ))
        }
    })
}

/// 内側の結果列のメタデータから静的な列型を決める。疑似列 `id` は `BIGINT`（算術で整数として扱う）。
/// 静的な型を持たない列（`Computed { ty: None }`）は `0A000`。
pub(crate) fn column_type_of_meta(meta: &ColumnMeta) -> Result<ColumnType, SqlSurfaceError> {
    match meta {
        ColumnMeta::Id | ColumnMeta::AliasedId { .. } => Ok(ColumnType::BigInt),
        ColumnMeta::Scalar { ty, .. } => Ok(ty.clone()),
        ColumnMeta::Computed { ty: Some(ty), .. } => Ok(ty.clone()),
        ColumnMeta::Computed { ty: None, .. } => Err(SqlSurfaceError::feature_not_supported(
            "this column type is not supported in a scalar subquery inside an expression",
        )),
    }
}

fn layout_error() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "correlated scalar value does not match its static type".to_string(),
    }
}

/// 整数を `f64` へ写せるか検査する。`2^53` を超える値は黙って丸めず `22003`（`ColumnRef` の評価と同じ規則）。
fn exact_scalar(magnitude: u64) -> Result<(), SqlSurfaceError> {
    if magnitude > MAX_EXACT_F64_INT {
        return Err(SqlSurfaceError::numeric_out_of_range(
            "integer value exceeds the range that can be exactly represented",
        ));
    }
    Ok(())
}

/// 内側の結果セルを式の値へ変換する。静的な型と食い違うセルは配線不備として `Internal`
/// （fail-closed。固定文言で値を含めない）。
fn cell_to_value(cell: &Cell, ty: &ColumnType) -> Result<ExprValue<'static>, SqlSurfaceError> {
    Ok(match (expr_type_for(ty)?, cell) {
        (_, Cell::Null) => ExprValue::Null,
        (ExprType::Scalar, Cell::Integer(v)) => {
            exact_scalar(*v)?;
            ExprValue::Scalar(*v as f64)
        }
        (ExprType::Scalar, Cell::SignedInteger(v)) => {
            exact_scalar(v.unsigned_abs())?;
            ExprValue::Scalar(*v as f64)
        }
        (ExprType::Scalar, Cell::Float(f)) => {
            if !f.is_finite() {
                return Err(SqlSurfaceError::invalid_input(
                    "numeric value is not finite",
                ));
            }
            ExprValue::Scalar(*f)
        }
        (ExprType::Bool, Cell::Bool(b)) => ExprValue::Bool(*b),
        (ExprType::Text, Cell::Text(s)) => ExprValue::Text(Cow::Owned(s.clone())),
        (ExprType::Date, Cell::Date(d)) => ExprValue::Date(*d),
        (ExprType::Timestamp, Cell::Timestamp(t)) => ExprValue::Timestamp(*t),
        (ExprType::Time, Cell::Time(t)) => ExprValue::Time(*t),
        (ExprType::TimestampTz, Cell::TimestampTz(t)) => ExprValue::TimestampTz(*t),
        (ExprType::Interval, Cell::Interval(v)) => ExprValue::Interval(*v),
        (ExprType::Numeric, Cell::Numeric(d)) => ExprValue::Numeric(*d),
        (ExprType::Uuid, Cell::Uuid(u)) => ExprValue::Uuid(*u),
        (ExprType::Bytea, Cell::Bytes(b)) => ExprValue::Bytes(Cow::Owned(b.clone())),
        _ => return Err(layout_error()),
    })
}
