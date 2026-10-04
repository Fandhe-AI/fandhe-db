//! JOIN の行値（[`Cell`]）を型クラスごとに扱う共通部品（Issue #925・#1190、
//! SQL-28・RLS-10、TASK-212）。
//!
//! 責務境界: `sql::join` の各段（`plan`・`exec`・`residual`・`aggregate`）が共有する
//! 「型クラスの判定」「結合キー・グループキーの正準バイト列化」「列同士の比較・
//! `ORDER BY` 用の比較値」「[`Cell`] → [`ScalarRef`] のアダプタ」を 1 か所に集約する。
//! 走査（RLS 適用）・予算計上はここでは行わない（呼び出し元の `sql::join::exec`
//! が可視行だけをここへ渡す）。比較の NaN／±0 規約は `sql::order_value::compare_f64`
//! を再利用し、単一テーブル経路の `ORDER BY` と食い違わないようにする。

use std::cmp::Ordering;

use crate::catalog::ColumnType;
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::order_value::compare_f64;
use crate::sql::udf_call::BinOp;

/// JOIN 結合キー・グループキーの型クラス（Issue #925 §2.3）。整数クラス（疑似列
/// `id`・`INTEGER`・`BIGINT`）は相互に結合できる（外部キー設計 `a.id = b.fk` 形を
/// 成立させるため）。それ以外は完全一致（`Enum` は型名まで一致）が必要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum JoinKeyClass {
    Integer,
    Text,
    Bool,
    Date,
    Timestamp,
    Uuid,
    Bytea,
    Enum(String),
}

/// 結合キーとして許可する列型を判定する（Issue #925 §2.3）。`None` は疑似列
/// `id`（整数クラス）。`VECTOR`・`REAL`／`DOUBLE`／`NUMERIC`／`JSON`／`JSONB`／
/// `ARRAY` は許可しない（`42601`）。
pub(super) fn key_class(ty: Option<&ColumnType>) -> Result<JoinKeyClass, SqlSurfaceError> {
    match ty {
        None => Ok(JoinKeyClass::Integer),
        Some(ColumnType::Integer) | Some(ColumnType::BigInt) => Ok(JoinKeyClass::Integer),
        Some(ColumnType::Text) => Ok(JoinKeyClass::Text),
        Some(ColumnType::Boolean) => Ok(JoinKeyClass::Bool),
        Some(ColumnType::Date) => Ok(JoinKeyClass::Date),
        Some(ColumnType::Timestamp) => Ok(JoinKeyClass::Timestamp),
        Some(ColumnType::Uuid) => Ok(JoinKeyClass::Uuid),
        Some(ColumnType::Bytea) => Ok(JoinKeyClass::Bytea),
        Some(ColumnType::Enum(def)) => Ok(JoinKeyClass::Enum(def.name().to_string())),
        Some(_) => Err(SqlSurfaceError::unsupported(
            "this column type cannot be used as a JOIN key",
        )),
    }
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SqlSurfaceError> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| SqlSurfaceError::payload_too_large("JOIN key exceeds length limit"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// 結合キー 1 成分（セル＋型クラス）を正準バイト列へ追記する（Issue #925
/// §2.4）。型クラスは束縛段の型検証を通過済みのため、`class` と `cell` の組み合わせは
/// 常に整合する（不整合は内部バグとして防御的に拒否する。fail-closed）。
pub(super) fn encode_key_component(
    cell: &Cell,
    class: &JoinKeyClass,
    out: &mut Vec<u8>,
) -> Result<(), SqlSurfaceError> {
    match (class, cell) {
        (JoinKeyClass::Integer, Cell::Integer(v)) => {
            out.extend_from_slice(&i128::from(*v).to_be_bytes());
            Ok(())
        }
        (JoinKeyClass::Integer, Cell::SignedInteger(v)) => {
            out.extend_from_slice(&i128::from(*v).to_be_bytes());
            Ok(())
        }
        (JoinKeyClass::Text, Cell::Text(s)) | (JoinKeyClass::Enum(_), Cell::Text(s)) => {
            push_len_prefixed(out, s.as_bytes())
        }
        (JoinKeyClass::Bool, Cell::Bool(b)) => {
            out.push(u8::from(*b));
            Ok(())
        }
        (JoinKeyClass::Date, Cell::Date(d)) => {
            out.extend_from_slice(&d.to_be_bytes());
            Ok(())
        }
        (JoinKeyClass::Timestamp, Cell::Timestamp(t)) => {
            out.extend_from_slice(&t.to_be_bytes());
            Ok(())
        }
        (JoinKeyClass::Uuid, Cell::Uuid(u)) => {
            out.extend_from_slice(u.as_bytes());
            Ok(())
        }
        (JoinKeyClass::Bytea, Cell::Bytes(b)) => push_len_prefixed(out, b),
        _ => Err(SqlSurfaceError::Internal {
            detail: "JOIN key cell/class mismatch".to_string(),
        }),
    }
}

/// 列同士の比較・スカラー `ORDER BY` の型クラス（Issue #1190）。結合キーと異なり
/// 浮動小数・`NUMERIC` も比較できる。クラスが一致しない比較は `42804`。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CmpClass {
    Integer,
    Float,
    Numeric,
    Text,
    Bool,
    Date,
    Timestamp,
    Uuid,
    Bytea,
    Enum { name: String, labels: Vec<String> },
}

/// 比較に使える列型か判定する（`None` は疑似列 `id`＝整数クラス）。`VECTOR`・
/// `JSON`・`ARRAY` は `None`（呼び出し元が文脈に応じた `wire_code` へ写像する）。
pub(crate) fn cmp_class(ty: Option<&ColumnType>) -> Option<CmpClass> {
    match ty {
        None => Some(CmpClass::Integer),
        Some(ColumnType::Integer) | Some(ColumnType::BigInt) => Some(CmpClass::Integer),
        Some(ColumnType::Real) | Some(ColumnType::Double) => Some(CmpClass::Float),
        Some(ColumnType::Numeric { .. }) => Some(CmpClass::Numeric),
        Some(ColumnType::Text) => Some(CmpClass::Text),
        Some(ColumnType::Boolean) => Some(CmpClass::Bool),
        Some(ColumnType::Date) => Some(CmpClass::Date),
        Some(ColumnType::Timestamp) => Some(CmpClass::Timestamp),
        Some(ColumnType::Uuid) => Some(CmpClass::Uuid),
        Some(ColumnType::Bytea) => Some(CmpClass::Bytea),
        Some(ColumnType::Enum(def)) => Some(CmpClass::Enum {
            name: def.name().to_string(),
            labels: def.labels().to_vec(),
        }),
        Some(_) => None,
    }
}

/// 比較用の値（[`Cell`] から型クラスに従って取り出した借用値）。整数クラスは
/// `id`（`u64`）と符号付き整数（`i64`）を混在させて比較できるよう `i128` に
/// 正規化する（`OrderValue::Id`／`SignedInt` の異種比較が `Equal` に倒れる実装を
/// 避けるため）。
#[derive(Debug, Clone, Copy)]
pub(crate) enum CmpVal<'a> {
    Int(i128),
    Float(f64),
    Num(crate::numeric::Decimal),
    Bytes(&'a [u8]),
    Bool(bool),
    Uuid(crate::uuid::Uuid),
    Ordinal(usize),
}

/// [`Cell`] を型クラスの比較値へ変換する。`NULL` は `None`。クラスとセルの
/// 不整合は内部バグとして `Internal`（fail-closed）。
pub(crate) fn cmp_val<'a>(
    cell: &'a Cell,
    class: &CmpClass,
) -> Result<Option<CmpVal<'a>>, SqlSurfaceError> {
    let v =
        match (class, cell) {
            (_, Cell::Null) => return Ok(None),
            (CmpClass::Integer, Cell::Integer(v)) => CmpVal::Int(i128::from(*v)),
            (CmpClass::Integer, Cell::SignedInteger(v)) => CmpVal::Int(i128::from(*v)),
            (CmpClass::Float, Cell::Float(v)) => CmpVal::Float(*v),
            (CmpClass::Numeric, Cell::Numeric(d)) => CmpVal::Num(*d),
            (CmpClass::Text, Cell::Text(s)) => CmpVal::Bytes(s.as_bytes()),
            (CmpClass::Bytea, Cell::Bytes(b)) => CmpVal::Bytes(b),
            (CmpClass::Bool, Cell::Bool(b)) => CmpVal::Bool(*b),
            (CmpClass::Date, Cell::Date(d)) => CmpVal::Int(i128::from(*d)),
            (CmpClass::Timestamp, Cell::Timestamp(t)) => CmpVal::Int(i128::from(*t)),
            (CmpClass::Uuid, Cell::Uuid(u)) => CmpVal::Uuid(*u),
            (CmpClass::Enum { labels, .. }, Cell::Text(s)) => {
                let ordinal = labels.iter().position(|l| l == s).ok_or_else(|| {
                    SqlSurfaceError::Internal {
                        detail: "JOIN ENUM value is not in the declared label set".to_string(),
                    }
                })?;
                CmpVal::Ordinal(ordinal)
            }
            _ => {
                return Err(SqlSurfaceError::Internal {
                    detail: "JOIN comparison cell/class mismatch".to_string(),
                })
            }
        };
    Ok(Some(v))
}

/// 同じクラス由来の 2 値の昇順比較。異なる variant は構築されない不変条件
/// （[`cmp_val`] がクラスから一意に variant を選ぶ）に反するため `Equal` に倒す。
pub(super) fn compare_vals(a: &CmpVal<'_>, b: &CmpVal<'_>) -> Ordering {
    match (a, b) {
        (CmpVal::Int(x), CmpVal::Int(y)) => x.cmp(y),
        (CmpVal::Float(x), CmpVal::Float(y)) => compare_f64(*x, *y),
        (CmpVal::Num(x), CmpVal::Num(y)) => crate::numeric::cmp_exact(x, y),
        (CmpVal::Bytes(x), CmpVal::Bytes(y)) => x.cmp(y),
        (CmpVal::Bool(x), CmpVal::Bool(y)) => x.cmp(y),
        (CmpVal::Uuid(x), CmpVal::Uuid(y)) => x.cmp(y),
        (CmpVal::Ordinal(x), CmpVal::Ordinal(y)) => x.cmp(y),
        _ => Ordering::Equal,
    }
}

/// `ORDER BY` 1 キー分の比較（PostgreSQL 既定: ASC は NULL 末尾・DESC は NULL 先頭。
/// 戻り値は「昇順に安定ソートすると最終的な出力順になる」意味の `Ordering`）。
pub(crate) fn compare_order(
    a: Option<&CmpVal<'_>>,
    b: Option<&CmpVal<'_>>,
    descending: bool,
) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if descending {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if descending {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(x), Some(y)) => {
            let base = compare_vals(x, y);
            if descending {
                base.reverse()
            } else {
                base
            }
        }
    }
}

/// 比較演算子（`= < <= > >=`）の適用。`Ordering` が定まらない（どちらかが
/// NULL）場合は呼び出し元が偽として扱う。算術演算子は構文段が生成しないため
/// 到達しないが、fail-closed に偽を返す。
pub(super) fn op_holds(op: BinOp, ord: Ordering) -> bool {
    match op {
        BinOp::Eq => ord == Ordering::Equal,
        BinOp::Lt => ord == Ordering::Less,
        BinOp::Le => ord != Ordering::Greater,
        BinOp::Gt => ord == Ordering::Greater,
        BinOp::Ge => ord != Ordering::Less,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => false,
    }
}

/// 述語束縛・集計の観測器が期待する借用形（[`ScalarRef`]）へ [`Cell`] を写像する
/// アダプタ。`ty` は列の宣言型（`Cell::SignedInteger` が `INTEGER` か `BIGINT` か、
/// `Cell::Float` が `REAL` か `DOUBLE` か、`Cell::Text` が `TEXT` か `ENUM` かは
/// セルだけでは決まらないため）。`NULL` は `Ok(None)`。値域外（`INTEGER` 列に
/// `i32` を超える値など）は保存データの不整合として `Internal`（fail-closed）。
/// 述語・集計が扱えない型（`VECTOR`・`ARRAY`）は `Ok(None)` ではなく `Internal`
/// にして、黙って NULL 扱い（fail-open な誤判定）にしない。
pub(crate) fn cell_scalar<'a>(
    cell: &'a Cell,
    ty: &ColumnType,
) -> Result<Option<ScalarRef<'a>>, SqlSurfaceError> {
    let bug = |detail: &str| SqlSurfaceError::Internal {
        detail: format!("JOIN cell adapter: {detail}"),
    };
    Ok(match (ty, cell) {
        (_, Cell::Null) => None,
        (ColumnType::Text, Cell::Text(s)) => Some(ScalarRef::Text(s)),
        (ColumnType::Enum(_), Cell::Text(s)) => Some(ScalarRef::Enum(s)),
        (ColumnType::Integer, Cell::SignedInteger(v)) => Some(ScalarRef::Integer(
            i32::try_from(*v).map_err(|_| bug("INTEGER value out of range"))?,
        )),
        (ColumnType::BigInt, Cell::SignedInteger(v)) => Some(ScalarRef::BigInt(*v)),
        (ColumnType::Real, Cell::Float(f)) => Some(ScalarRef::Real(*f as f32)),
        (ColumnType::Double, Cell::Float(f)) => Some(ScalarRef::Double(*f)),
        (ColumnType::Boolean, Cell::Bool(b)) => Some(ScalarRef::Bool(*b)),
        (ColumnType::Date, Cell::Date(d)) => Some(ScalarRef::Date(*d)),
        (ColumnType::Timestamp, Cell::Timestamp(t)) => Some(ScalarRef::Timestamp(*t)),
        (ColumnType::Numeric { .. }, Cell::Numeric(d)) => Some(ScalarRef::Numeric(*d)),
        (ColumnType::Uuid, Cell::Uuid(u)) => Some(ScalarRef::Uuid(*u)),
        (ColumnType::Bytea, Cell::Bytes(b)) => Some(ScalarRef::Bytes(b)),
        (ColumnType::Json, Cell::Json(s)) => Some(ScalarRef::Json(s)),
        _ => return Err(bug("unsupported column type or cell variant")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_class_mixes_id_and_signed_values() {
        let id = Cell::Integer(u64::MAX);
        let signed = Cell::SignedInteger(-1);
        let a = cmp_val(&id, &CmpClass::Integer).unwrap().unwrap();
        let b = cmp_val(&signed, &CmpClass::Integer).unwrap().unwrap();
        assert_eq!(compare_vals(&a, &b), Ordering::Greater);
        let same_a = Cell::Integer(7);
        let same_b = Cell::SignedInteger(7);
        let a = cmp_val(&same_a, &CmpClass::Integer).unwrap().unwrap();
        let b = cmp_val(&same_b, &CmpClass::Integer).unwrap().unwrap();
        assert_eq!(compare_vals(&a, &b), Ordering::Equal);
    }

    #[test]
    fn order_places_nulls_last_for_asc_and_first_for_desc() {
        let one = CmpVal::Int(1);
        assert_eq!(compare_order(Some(&one), None, false), Ordering::Less);
        assert_eq!(compare_order(Some(&one), None, true), Ordering::Greater);
        assert_eq!(compare_order(None, None, true), Ordering::Equal);
    }

    #[test]
    fn op_holds_covers_the_supported_operators() {
        assert!(op_holds(BinOp::Le, Ordering::Equal));
        assert!(op_holds(BinOp::Lt, Ordering::Less));
        assert!(!op_holds(BinOp::Gt, Ordering::Equal));
        assert!(!op_holds(BinOp::Add, Ordering::Equal));
    }

    #[test]
    fn cell_scalar_rejects_out_of_range_integer_and_maps_null() {
        assert!(cell_scalar(&Cell::Null, &ColumnType::Integer)
            .unwrap()
            .is_none());
        assert!(cell_scalar(&Cell::SignedInteger(i64::MAX), &ColumnType::Integer).is_err());
    }
}
