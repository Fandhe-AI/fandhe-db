//! 集計入力の行ごとの評価ステージ（Issue #1808。親 #1807・SQL-57 ポインタ）。
//!
//! 役割: [`crate::sql::aggregate::Accumulator::observe`] 系が「走査中の 1 行から値を
//! 取り出す処理」と「アキュムレータへ値を渡す処理」を分けるための前段。
//! [`eval_aggregate_input`] が [`AggregateInput`] を 1 行分の [`AggregateRowValue`] に
//! 評価し、アキュムレータ側（`observe_value`／`observe_distinct`）は評価結果だけを
//! 受け取る。列参照は「評価結果が列の値そのものになる式」の特殊ケースとして
//! 同じ中間値へ写す。式引数（#1809）は、式の評価結果をこの中間値へ変換する腕を
//! 足すだけで既存の観測経路へ合流できる。
//!
//! 呼び出し文脈: `sql::aggregate`／`group_by`／`join::aggregate`／`window` の行ループが
//! RLS 可視性判定と TABLE-12 のテナント整合検査を通過した行についてだけ
//! `Accumulator::observe` を呼び、その内部から本モジュールが呼ばれる（呼び出し位置は
//! 変えていない）。型不一致は従来どおり [`accumulator_bug`]（XX000）で fail-closed に
//! 拒否し、受信データ経路のため添字アクセス・`unwrap` は使わない。

use crate::datetime::Interval;
use crate::row_codec::ScalarRef;
use crate::sql::aggregate::{accumulator_bug, RowVector};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::expr_program::StackValue;
use crate::sql::parser::AggregateInput;
use crate::sql::udf_call::{self, ExprValue};

/// 非 NULL を数えるだけの列型（`COUNT` 専用）。非 distinct 経路は型を確かめず
/// 数え、`COUNT(DISTINCT)` 経路だけが期待する型を確かめてキーを作る。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CountOnlyKind {
    Bool,
    Bytea,
    Enum,
    Uuid,
}

/// 1 行分の集計入力の評価結果。アキュムレータの入口（`observe_*`）と distinct
/// キーに 1 対 1 で対応させ、情報を落とさない。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum AggregateRowValue<'a> {
    /// NULL（その行は集計対象外）。
    Null,
    /// 中身を見ない非 NULL（`*`・VECTOR 列の存在・ARRAY／JSON 列）。
    Present,
    Id(u64),
    Text(&'a str),
    /// `INTEGER`／`SMALLINT`。distinct キー長（4 バイト）を保つため BigInt と分ける。
    Int32(i32),
    Int64(i64),
    Float(f64),
    /// scale 検査済みの unscaled 値。
    Numeric(i128),
    Date(i32),
    Timestamp(i64),
    Time(i64),
    TimestampTz(i64),
    Interval(Interval),
    /// `COUNT` 専用列の生の値（distinct 経路でだけ型を確かめる）。
    CountOnly {
        kind: CountOnlyKind,
        value: ScalarRef<'a>,
    },
}

/// `input` を 1 行分評価する。`scanned`／`vector`／`scratch` の意味は
/// [`crate::sql::aggregate::Accumulator::observe`] と同じ。
pub(crate) fn eval_aggregate_input<'a>(
    input: &AggregateInput,
    id: u64,
    vector: &RowVector<'a>,
    scanned: &'a [Option<ScalarRef<'a>>],
    scratch: &mut Vec<StackValue>,
) -> Result<AggregateRowValue<'a>, SqlSurfaceError> {
    use AggregateRowValue as V;
    let cell = |index: &usize| scanned.get(*index).copied().flatten();
    let count_only = |index: &usize, kind: CountOnlyKind| match cell(index) {
        Some(value) => V::CountOnly { kind, value },
        None => V::Null,
    };
    Ok(match input {
        AggregateInput::StringAgg(_) => {
            return Err(accumulator_bug(
                "string_agg input observed by a non-string_agg accumulator",
            ))
        }
        AggregateInput::ArrayAgg(_) => {
            return Err(accumulator_bug(
                "array_agg input observed by a non-array_agg accumulator",
            ))
        }
        AggregateInput::AllVisible => V::Present,
        // `row.embedding` が空 = 未設定（NULL）という既存契約。`dim` だけで判定する。
        AggregateInput::VectorColumnPresence => {
            if vector.dim == 0 {
                V::Null
            } else {
                V::Present
            }
        }
        AggregateInput::IdU64 => V::Id(id),
        // TEXT 以外は NULL 扱い（従来どおりの寛容さ）。
        AggregateInput::TextColumn(index) | AggregateInput::CollatedTextColumn { index, .. } => {
            match cell(index).and_then(|v| v.as_text()) {
                Some(s) => V::Text(s),
                None => V::Null,
            }
        }
        AggregateInput::BooleanColumn(index) => count_only(index, CountOnlyKind::Bool),
        AggregateInput::ByteaColumn(index) => count_only(index, CountOnlyKind::Bytea),
        AggregateInput::EnumColumn(index) => count_only(index, CountOnlyKind::Enum),
        AggregateInput::UuidColumn(index) => count_only(index, CountOnlyKind::Uuid),
        AggregateInput::ArrayColumn(index) | AggregateInput::JsonColumn(index) => {
            if cell(index).is_some() {
                V::Present
            } else {
                V::Null
            }
        }
        AggregateInput::DateColumn(index) => match cell(index) {
            Some(v) => V::Date(v.as_date().ok_or_else(|| {
                accumulator_bug("DateColumn observed a ScalarRef that is not Date")
            })?),
            None => V::Null,
        },
        AggregateInput::TimestampColumn(index) => match cell(index) {
            Some(v) => V::Timestamp(v.as_timestamp().ok_or_else(|| {
                accumulator_bug("TimestampColumn observed a ScalarRef that is not Timestamp")
            })?),
            None => V::Null,
        },
        AggregateInput::TimeColumn(index) => match cell(index) {
            Some(v) => V::Time(v.as_time().ok_or_else(|| {
                accumulator_bug("TimeColumn observed a ScalarRef that is not Time")
            })?),
            None => V::Null,
        },
        AggregateInput::IntervalColumn(index) => match cell(index) {
            Some(v) => V::Interval(v.as_interval().ok_or_else(|| {
                accumulator_bug("IntervalColumn observed a ScalarRef that is not Interval")
            })?),
            None => V::Null,
        },
        AggregateInput::TimestampTzColumn(index) => match cell(index) {
            Some(v) => V::TimestampTz(v.as_timestamptz().ok_or_else(|| {
                accumulator_bug("TimestampTzColumn observed a ScalarRef that is not TimestampTz")
            })?),
            None => V::Null,
        },
        AggregateInput::IntegerColumn(index) | AggregateInput::SmallIntColumn(index) => {
            match cell(index) {
                Some(ScalarRef::Integer(v)) => V::Int32(v),
                Some(_) => {
                    return Err(accumulator_bug(
                        "IntegerColumn observed a ScalarRef that is not Integer",
                    ))
                }
                None => V::Null,
            }
        }
        AggregateInput::BigIntColumn(index) => match cell(index) {
            Some(ScalarRef::BigInt(v)) => V::Int64(v),
            Some(_) => {
                return Err(accumulator_bug(
                    "BigIntColumn observed a ScalarRef that is not BigInt",
                ))
            }
            None => V::Null,
        },
        AggregateInput::RealColumn(index) => match cell(index) {
            Some(ScalarRef::Real(v)) => V::Float(f64::from(v)),
            Some(_) => {
                return Err(accumulator_bug(
                    "RealColumn observed a ScalarRef that is not Real",
                ))
            }
            None => V::Null,
        },
        AggregateInput::DoubleColumn(index) => match cell(index) {
            Some(ScalarRef::Double(v)) => V::Float(v),
            Some(_) => {
                return Err(accumulator_bug(
                    "DoubleColumn observed a ScalarRef that is not Double",
                ))
            }
            None => V::Null,
        },
        // 格納された Decimal の scale は列の scale と一致する契約。不一致は
        // デコード側の実装バグとして fail-closed に拒否する。
        AggregateInput::NumericColumn { index, scale, .. } => match cell(index) {
            Some(ScalarRef::Numeric(d)) => {
                if d.scale() != *scale {
                    return Err(accumulator_bug(
                        "NumericColumn observed a Decimal whose scale does not match the column",
                    ));
                }
                V::Numeric(d.unscaled())
            }
            Some(_) => {
                return Err(accumulator_bug(
                    "NumericColumn observed a ScalarRef that is not Numeric",
                ))
            }
            None => V::Null,
        },
        AggregateInput::ScalarExpr { source, program } => {
            // 式木が実際に embedding へ到達する場合のみ `vector.values` を要求する
            // （`ReferencedColumns::derive` と同じ判定関数）。未デコードは導出漏れという
            // 実装バグとして fail-closed に拒否する。
            let embedding: &[f32] = if udf_call::references_embedding(source) {
                match vector.values {
                    Some(v) => v,
                    None => {
                        return Err(accumulator_bug(
                            "ScalarExpr references the VECTOR column but the row's embedding was not decoded",
                        ))
                    }
                }
            } else {
                &[]
            };
            match program.eval(id, embedding, scanned, scratch)? {
                ExprValue::Scalar(v) => V::Float(v),
                // NULL は集計対象から除外する（SQL-26 の共有契約）。
                ExprValue::Null => V::Null,
                // 束縛段が `ExprType::Scalar` のみを束縛するため到達しない。
                _ => {
                    return Err(accumulator_bug(
                        "scalar-typed BoundExpr evaluated to a non-scalar value",
                    ))
                }
            }
        }
    })
}

impl AggregateRowValue<'_> {
    /// `COUNT(DISTINCT)` の正準化キー（SQL-25 (c)・TASK-209）。バイト表現は
    /// `DistinctBudget` の計上量を決めるため変えてはならない。`Null` は `None`。
    pub(crate) fn distinct_key(&self) -> Result<Option<Vec<u8>>, SqlSurfaceError> {
        use crate::sql::distinct::canon_f64;
        Ok(match self {
            AggregateRowValue::Null => None,
            AggregateRowValue::Present => {
                return Err(accumulator_bug(
                    "COUNT(DISTINCT) accumulator observed an input type that bind-time resolution should have rejected",
                ))
            }
            AggregateRowValue::Id(id) => Some(id.to_be_bytes().to_vec()),
            AggregateRowValue::Text(s) => Some(s.as_bytes().to_vec()),
            AggregateRowValue::Int32(v) => Some(v.to_be_bytes().to_vec()),
            AggregateRowValue::Int64(v) => Some(v.to_be_bytes().to_vec()),
            AggregateRowValue::Float(v) => Some(canon_f64(*v).to_vec()),
            AggregateRowValue::Numeric(v) => Some(v.to_be_bytes().to_vec()),
            AggregateRowValue::Date(v) => Some(v.to_be_bytes().to_vec()),
            AggregateRowValue::Timestamp(v)
            | AggregateRowValue::Time(v)
            | AggregateRowValue::TimestampTz(v) => Some(v.to_be_bytes().to_vec()),
            // 正規化キー（16 バイト）。`1 mon` と `30 days` は同じキー（Issue #1580）。
            AggregateRowValue::Interval(v) => {
                Some(crate::datetime::interval_cmp_key(v).to_be_bytes().to_vec())
            }
            AggregateRowValue::CountOnly { kind, value } => Some(match (kind, value) {
                (CountOnlyKind::Bool, ScalarRef::Bool(b)) => vec![u8::from(*b)],
                (CountOnlyKind::Bytea, ScalarRef::Bytes(b)) => b.to_vec(),
                (CountOnlyKind::Enum, ScalarRef::Enum(s)) => s.as_bytes().to_vec(),
                (CountOnlyKind::Uuid, ScalarRef::Uuid(u)) => u.as_bytes().to_vec(),
                (kind, _) => {
                    return Err(accumulator_bug(match kind {
                        CountOnlyKind::Bool => {
                            "BooleanColumn observed a ScalarRef that is not Bool (distinct)"
                        }
                        CountOnlyKind::Bytea => {
                            "ByteaColumn observed a ScalarRef that is not Bytes (distinct)"
                        }
                        CountOnlyKind::Enum => {
                            "EnumColumn observed a ScalarRef that is not Enum (distinct)"
                        }
                        CountOnlyKind::Uuid => {
                            "UuidColumn observed a ScalarRef that is not Uuid (distinct)"
                        }
                    }))
                }
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    //! 評価ステージの回帰テスト（Issue #1808。SQL-57 ポインタ）。型検査の厳しさと
    //! distinct キーのバイト長が従来どおりであることを固定する。
    use super::*;

    const VEC: RowVector<'static> = RowVector {
        dim: 0,
        values: None,
    };

    fn eval<'a>(
        input: &AggregateInput,
        scanned: &'a [Option<ScalarRef<'a>>],
    ) -> Result<AggregateRowValue<'a>, SqlSurfaceError> {
        eval_aggregate_input(input, 7, &VEC, scanned, &mut Vec::new())
    }

    #[test]
    fn typed_columns_map_to_values_and_null() {
        let row = [
            Some(ScalarRef::Integer(5)),
            None,
            Some(ScalarRef::BigInt(9)),
        ];
        assert_eq!(
            eval(&AggregateInput::IntegerColumn(0), &row).unwrap(),
            AggregateRowValue::Int32(5)
        );
        assert_eq!(
            eval(&AggregateInput::IntegerColumn(1), &row).unwrap(),
            AggregateRowValue::Null
        );
        assert_eq!(
            eval(&AggregateInput::BigIntColumn(2), &row).unwrap(),
            AggregateRowValue::Int64(9)
        );
        assert_eq!(
            eval(&AggregateInput::IdU64, &row).unwrap(),
            AggregateRowValue::Id(7)
        );
    }

    #[test]
    fn type_mismatch_is_fail_closed_but_text_is_lenient() {
        let row = [Some(ScalarRef::Text("a")), Some(ScalarRef::Integer(1))];
        assert!(eval(&AggregateInput::IntegerColumn(0), &row).is_err());
        assert!(eval(&AggregateInput::BigIntColumn(1), &row).is_err());
        assert_eq!(
            eval(&AggregateInput::TextColumn(1), &row).unwrap(),
            AggregateRowValue::Null
        );
        assert_eq!(
            eval(&AggregateInput::TextColumn(0), &row).unwrap(),
            AggregateRowValue::Text("a")
        );
    }

    #[test]
    fn count_only_defers_type_check_to_distinct() {
        let row = [Some(ScalarRef::Integer(1))];
        let v = eval(&AggregateInput::BooleanColumn(0), &row).unwrap();
        assert!(matches!(v, AggregateRowValue::CountOnly { .. }));
        assert!(v.distinct_key().is_err());
        let ok = AggregateRowValue::CountOnly {
            kind: CountOnlyKind::Bool,
            value: ScalarRef::Bool(true),
        };
        assert_eq!(ok.distinct_key().unwrap(), Some(vec![1]));
    }

    #[test]
    fn distinct_key_lengths_are_unchanged() {
        let len = |v: AggregateRowValue<'_>| v.distinct_key().unwrap().map(|k| k.len());
        assert_eq!(len(AggregateRowValue::Int32(1)), Some(4));
        assert_eq!(len(AggregateRowValue::Int64(1)), Some(8));
        assert_eq!(len(AggregateRowValue::Id(1)), Some(8));
        assert_eq!(len(AggregateRowValue::Numeric(1)), Some(16));
        assert_eq!(len(AggregateRowValue::Date(1)), Some(4));
        assert_eq!(len(AggregateRowValue::Time(1)), Some(8));
        assert_eq!(len(AggregateRowValue::Null), None);
        assert!(AggregateRowValue::Present.distinct_key().is_err());
    }

    #[test]
    fn vector_presence_follows_dim() {
        let present = RowVector {
            dim: 3,
            values: None,
        };
        let mut s = Vec::new();
        assert_eq!(
            eval_aggregate_input(
                &AggregateInput::VectorColumnPresence,
                1,
                &present,
                &[],
                &mut s
            )
            .unwrap(),
            AggregateRowValue::Present
        );
        assert_eq!(
            eval(&AggregateInput::VectorColumnPresence, &[]).unwrap(),
            AggregateRowValue::Null
        );
    }
}
