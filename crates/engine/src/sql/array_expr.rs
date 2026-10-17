//! 配列コンストラクタ `ARRAY[...]` と添字 `arr[i]`／スライス `arr[lo:hi]` の束縛・評価
//! （対象ビヘイビア: TABLE-35。Issue #1602 のポインタ）。
//!
//! 責務境界: 構文段（`sql::allowlist::Parser`）が作る [`Expr::ArrayConstructor`]・
//! [`Expr::Subscript`] を、束縛段（`sql::udf_call::bind_expr_any`）がここの
//! [`bind_array_constructor`]・[`bind_subscript`] で [`BoundExpr`] へ落とす。評価は
//! `sql::udf_call::eval_with_scalars` がここの `eval_*` へ委ねる（意味論を 1 実装に保つ）。
//!
//! 要素型の統一は PostgreSQL の共通型規則の部分集合で、暗黙・代入変換で到達できる型だけを
//! 結果にする。各要素の変換は `sql::cast::bind_cast` に委ね（第 2 の変換実装を作らない）、
//! 本モジュールは「どの型へ寄せるか」の決定と「寄せてはならない組を拒否する」判定だけを持つ。
//! 種別を静的に決められない数値・numeric（位取りを持てない）・smallint 配列・時刻系・
//! 入れ子の配列（多次元）は、黙って別の型として評価せず `0A000` で拒否する（fail-closed）。
//! 実装の無い組は `42804`（カテゴリ不一致）／`0A000`（未対応）に分けて返す。

use crate::catalog::{ArrayElemType, MAX_ARRAY_ELEMENTS};
use crate::row_codec::ArrayValue;
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::cast::{
    bind_cast, bind_operand, cast_bound, exceeds_exact_integer, number_literal_kind,
    source_cast_kind, CastKind, CastType, SourceKind,
};
use crate::sql::cast_value::{array_to_values, values_to_array};
use crate::sql::udf_call::{ArraySubscript, BindEnv, BoundExpr, Expr, ExprType, ExprValue};

/// 要素 1 個の分類（共通型の決定入力）。
pub(super) enum ItemClass {
    /// 裸の `NULL`（どの型にもなれる）。
    Null,
    /// 型なし文字列リテラル（PG の unknown。決まった型の入力として解釈する）。
    Untyped,
    /// 静的な型が AST だけで確定した要素（リテラル・明示 CAST）。束縛は変換時に [`bind_cast`] が行う。
    Kind(CastKind),
    /// 分類のために束縛した要素。変換時に再束縛しない（入れ子の式を束縛し直すと入れ子の深さに
    /// 対して指数的に束縛が重複し、ノード予算を迂回した CPU 消費になるため）。
    Bound {
        kind: CastKind,
        bound: BoundExpr,
        ty: ExprType,
    },
}

impl ItemClass {
    /// 共通型の決定に使う種別（`NULL`・型なしリテラルは持たない）。
    pub(super) fn kind(&self) -> Option<CastKind> {
        match self {
            ItemClass::Null | ItemClass::Untyped => None,
            ItemClass::Kind(k) | ItemClass::Bound { kind: k, .. } => Some(*k),
        }
    }
}

/// 配列に収められない型・組を示す `0A000`。
pub(super) fn unsupported(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: detail.to_string(),
    }
}

/// 要素 1 個を分類する。AST だけで型が決まるもの（リテラル・明示 CAST）以外は 1 度だけ束縛して
/// 静的な型を得て、その束縛結果を [`ItemClass::Bound`] に持たせる（ノード予算は実の予算を消費する）。
pub(super) fn classify_item(
    item: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<ItemClass, SqlSurfaceError> {
    match item {
        Expr::Null => return Ok(ItemClass::Null),
        Expr::String(_) => return Ok(ItemClass::Untyped),
        Expr::Number(raw) => {
            let kind = if exceeds_exact_integer(raw) {
                CastKind::Numeric
            } else {
                number_literal_kind(raw)
            };
            return Ok(ItemClass::Kind(kind));
        }
        Expr::Cast { ty, .. } => {
            return match ty {
                CastType::Array(_) => Err(unsupported(
                    "multidimensional arrays are not supported in an ARRAY constructor",
                )),
                other => Ok(ItemClass::Kind(other.kind())),
            }
        }
        _ => {}
    }
    let (bound, ty) = bind_operand(item, env, node_budget)?;
    match source_cast_kind(item, &bound, ty, env.schema()) {
        SourceKind::Known(kind) => Ok(ItemClass::Bound { kind, bound, ty }),
        SourceKind::Array(_) => Err(unsupported(
            "multidimensional arrays are not supported in an ARRAY constructor",
        )),
        SourceKind::UnknownNumeric | SourceKind::Outside => Err(unsupported(
            "an ARRAY element whose type cannot be determined statically is not supported yet",
        )),
    }
}

/// 要素の種別の集合から共通の要素型を決める（PG の `select_common_type` の部分集合）。
pub(super) fn common_element_type(kinds: &[CastKind]) -> Result<CastType, SqlSurfaceError> {
    use CastKind as K;
    let Some(&first) = kinds.first() else {
        // 型を持つ要素が無い（`ARRAY[NULL]`・`ARRAY['a']`）。PG は unknown を text に解決する。
        return Ok(CastType::Text);
    };
    let group = |k: K| match k {
        K::Int2 | K::Int4 | K::Int8 | K::Float4 | K::Float8 | K::Numeric => 0u8,
        K::Text | K::Varchar | K::Bpchar => 1,
        K::Date | K::Timestamp => 2,
        other => 3u8.saturating_add(other as u8),
    };
    if let Some(&other) = kinds.iter().find(|k| group(**k) != group(first)) {
        return Err(SqlSurfaceError::datatype_mismatch(format!(
            "ARRAY types {} and {} cannot be matched",
            first.label(),
            other.label()
        )));
    }
    let has = |k: K| kinds.contains(&k);
    match group(first) {
        0 => {
            if has(K::Numeric) {
                Err(unsupported(
                    "ARRAY of numeric elements is not supported yet; cast the elements to a float type",
                ))
            } else if has(K::Float8) {
                Ok(CastType::Float8)
            } else if has(K::Float4) {
                Ok(CastType::Float4)
            } else if has(K::Int8) {
                Ok(CastType::Int8)
            } else if has(K::Int4) {
                Ok(CastType::Int4)
            } else {
                Err(unsupported(
                    "ARRAY of smallint elements is not supported yet",
                ))
            }
        }
        1 => {
            if has(K::Text) || has(K::Varchar) {
                Ok(CastType::Text)
            } else if kinds.iter().all(|k| *k == K::Bpchar) {
                Err(unsupported(
                    "ARRAY of character(n) elements is not supported yet",
                ))
            } else {
                Ok(CastType::Text)
            }
        }
        2 => Ok(if has(K::Timestamp) {
            CastType::Timestamp
        } else {
            CastType::Date
        }),
        _ => match first {
            K::Bool => Ok(CastType::Bool),
            K::Json => Ok(CastType::Json),
            K::Jsonb => Ok(CastType::Jsonb),
            K::Uuid => Ok(CastType::Uuid),
            K::Bytea => Ok(CastType::Bytea),
            _ => Err(unsupported(
                "ARRAY of this element type is not supported yet",
            )),
        },
    }
}

/// `ARRAY[<e1>, ...]` の束縛（`udf_call::bind_expr_any` の `Expr::ArrayConstructor` 腕から呼ばれる。
/// 呼び出し側が構築ノード自身の 1 ノード分を課金済み）。
///
/// 手順は「各要素の分類 → 共通型の決定 → 各要素を共通型へ `bind_cast`」。暗黙・代入変換で届かない
/// 組（text と integer 等）は分類の段で `42804` にするため、明示 CAST だけで許される変換
/// （text → integer 等）が紛れ込むことはない。空の `ARRAY[]` は型を決められず拒否する
/// （`CAST(ARRAY[] AS t[])` は `bind_cast` が先に処理する）。
pub(super) fn bind_array_constructor(
    items: &[Expr],
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if items.is_empty() {
        return Err(SqlSurfaceError::datatype_mismatch(
            "cannot determine type of empty array; cast it explicitly, e.g. ARRAY[]::integer[]",
        ));
    }
    if items.len() > MAX_ARRAY_ELEMENTS as usize {
        return Err(SqlSurfaceError::payload_too_large(
            "ARRAY constructor has too many elements",
        ));
    }
    if crate::sql::udf_call::in_check_binding() {
        return Err(unsupported(
            "an ARRAY constructor is not supported in a CHECK constraint",
        ));
    }
    let mut classes: Vec<ItemClass> = Vec::with_capacity(items.len());
    for item in items {
        classes.push(classify_item(item, env, node_budget)?);
    }
    let kinds: Vec<CastKind> = classes.iter().filter_map(ItemClass::kind).collect();
    let target = common_element_type(&kinds)?;
    let array_ty = CastType::Array(Box::new(target.clone()));
    let Some(elem) = array_ty.array_elem_type() else {
        return Err(unsupported(
            "ARRAY of this element type is not supported yet",
        ));
    };
    let mut bound_items = Vec::with_capacity(items.len());
    for (item, class) in items.iter().zip(classes) {
        let (bound, _ty) = match class {
            ItemClass::Bound { bound, ty, .. } => {
                cast_bound(item, bound, ty, &target, env, node_budget)?
            }
            ItemClass::Null | ItemClass::Untyped | ItemClass::Kind(_) => {
                bind_cast(item, &target, env, node_budget)?
            }
        };
        bound_items.push(bound);
    }
    Ok((
        BoundExpr::ArrayConstructor {
            elem,
            items: bound_items,
        },
        ExprType::Array,
    ))
}

/// 空の `ARRAY[]` を変換先の要素型の空配列にする（`CAST(ARRAY[] AS t[])`・`ARRAY[]::t[]`。
/// `bind_cast` の被演算子が空のコンストラクタのときだけ呼ばれる）。
pub(super) fn bind_empty_array_as(target: &CastType) -> Result<BoundExpr, SqlSurfaceError> {
    match target.array_elem_type() {
        Some(elem) => Ok(BoundExpr::ArrayConstructor {
            elem,
            items: Vec::new(),
        }),
        None => Err(unsupported(
            "ARRAY of this element type is not supported yet",
        )),
    }
}

/// 要素型に対応する式の型（添字の結果型）。
fn elem_expr_type(elem: ArrayElemType) -> Result<ExprType, SqlSurfaceError> {
    crate::sql::cast::elem_cast_type(elem)
        .and_then(|t| t.expr_type())
        .ok_or_else(|| unsupported("subscripting an array of this element type is not supported"))
}

/// 添字・境界の式を int4 へ束縛する。整数型（smallint／integer／bigint）・整数リテラル・型なし
/// 文字列リテラル・`NULL` だけを受理し、float／numeric は丸めの意味論を持ち込まず `0A000`、
/// それ以外の型は PG と同じ `42804`。
pub(super) fn bind_subscript_index(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<BoundExpr, SqlSurfaceError> {
    let not_integer =
        || SqlSurfaceError::datatype_mismatch("array subscript must have type integer");
    match expr {
        Expr::Null | Expr::String(_) => {}
        Expr::Number(raw) => {
            if exceeds_exact_integer(raw) || number_literal_kind(raw) == CastKind::Numeric {
                return Err(unsupported(
                    "a non-integer array subscript is not supported yet",
                ));
            }
        }
        other => {
            // 1 度だけ束縛し、その結果を再束縛せずに int4 へ変換する（入れ子の添字式で束縛が
            // 指数的に重複しないようにする）。
            let (bound, ty) = bind_operand(other, env, node_budget)?;
            match source_cast_kind(other, &bound, ty, env.schema()) {
                SourceKind::Known(CastKind::Int2 | CastKind::Int4 | CastKind::Int8) => {}
                SourceKind::Known(CastKind::Float4 | CastKind::Float8 | CastKind::Numeric)
                | SourceKind::UnknownNumeric => {
                    return Err(unsupported(
                        "an array subscript whose integer type cannot be determined is not supported yet",
                    ))
                }
                _ => return Err(not_integer()),
            }
            let (bound, _) = cast_bound(other, bound, ty, &CastType::Int4, env, node_budget)?;
            return Ok(bound);
        }
    }
    let (bound, _) = bind_cast(expr, &CastType::Int4, env, node_budget)?;
    Ok(bound)
}

/// `<base>[<i>]`・`<base>[<lo>:<hi>]` の束縛（`udf_call::bind_expr_any` の `Expr::Subscript` 腕から
/// 呼ばれる。呼び出し側が構築ノード自身の 1 ノード分を課金済み）。
///
/// 基底は要素型を静的に確定できる配列（配列列・CAST の結果・コンストラクタ・スライス）に限る。
/// 配列でない基底は `42804`、jsonb の添字・ENUM 要素の配列は `0A000`（fail-closed）。
pub(super) fn bind_subscript(
    base: &Expr,
    subscript: &ArraySubscript,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if crate::sql::udf_call::in_check_binding() {
        return Err(unsupported(
            "an array subscript is not supported in a CHECK constraint",
        ));
    }
    let (bound_base, base_ty) = bind_operand(base, env, node_budget)?;
    let elem = match source_cast_kind(base, &bound_base, base_ty, env.schema()) {
        SourceKind::Array(Some(elem_ty)) => CastType::Array(Box::new(elem_ty))
            .array_elem_type()
            .ok_or_else(|| {
                unsupported("subscripting an array of this element type is not supported")
            })?,
        SourceKind::Array(None) => {
            return Err(unsupported(
                "subscripting an array of this element type is not supported",
            ))
        }
        SourceKind::Known(CastKind::Jsonb | CastKind::Json) => {
            return Err(unsupported(
                "subscripting a json value is not supported yet",
            ))
        }
        SourceKind::Known(kind) => {
            return Err(SqlSurfaceError::datatype_mismatch(format!(
                "cannot subscript type {} because it does not support subscripting",
                kind.label()
            )))
        }
        SourceKind::UnknownNumeric | SourceKind::Outside => {
            return Err(SqlSurfaceError::datatype_mismatch(
                "cannot subscript a value that is not an array",
            ))
        }
    };
    let base_box = Box::new(bound_base);
    match subscript {
        ArraySubscript::Index(index) => {
            let ty = elem_expr_type(elem)?;
            let index = bind_subscript_index(index, env, node_budget)?;
            Ok((
                BoundExpr::ArrayIndex {
                    elem,
                    base: base_box,
                    index: Box::new(index),
                },
                ty,
            ))
        }
        ArraySubscript::Slice { lo, hi } => {
            let lo = lo
                .as_deref()
                .map(|e| bind_subscript_index(e, env, node_budget).map(Box::new))
                .transpose()?;
            let hi = hi
                .as_deref()
                .map(|e| bind_subscript_index(e, env, node_budget).map(Box::new))
                .transpose()?;
            Ok((
                BoundExpr::ArraySlice {
                    elem,
                    base: base_box,
                    lo,
                    hi,
                },
                ExprType::Array,
            ))
        }
    }
}

/// 配列要素として複製される可変長ペイロードのバイト長（複製前の累計上限検査用）。
pub(super) fn payload_len(v: &ExprValue<'_>) -> usize {
    match v {
        ExprValue::Text(s) | ExprValue::Json(s) | ExprValue::JsonText(s) => s.len(),
        ExprValue::Bytes(b) => b.len(),
        _ => 0,
    }
}

/// 評価値を所有型へ写す（配列の要素として保持するため）。
pub(super) fn own(v: ExprValue<'_>) -> Result<ExprValue<'static>, SqlSurfaceError> {
    use std::borrow::Cow;
    Ok(match v {
        ExprValue::Scalar(x) => ExprValue::Scalar(x),
        ExprValue::Bool(b) => ExprValue::Bool(b),
        ExprValue::Text(s) => ExprValue::Text(Cow::Owned(s.into_owned())),
        ExprValue::Date(d) => ExprValue::Date(d),
        ExprValue::Timestamp(t) => ExprValue::Timestamp(t),
        ExprValue::Json(s) => ExprValue::Json(Cow::Owned(s.into_owned())),
        ExprValue::JsonText(s) => ExprValue::JsonText(Cow::Owned(s.into_owned())),
        ExprValue::Numeric(d) => ExprValue::Numeric(d),
        ExprValue::Uuid(u) => ExprValue::Uuid(u),
        ExprValue::Bytes(b) => ExprValue::Bytes(Cow::Owned(b.into_owned())),
        ExprValue::Null => ExprValue::Null,
        ExprValue::Vector(_)
        | ExprValue::Time(_)
        | ExprValue::TimestampTz(_)
        | ExprValue::Interval(_)
        | ExprValue::Array(_) => {
            return Err(SqlSurfaceError::Internal {
                detail: "array element evaluated to a type that cannot be stored".to_string(),
            })
        }
    })
}

/// 評価値の整数（int4 へ変換済みの添字・境界）を `i64` へ。NULL は `None`。
pub(super) fn index_value(v: &ExprValue<'_>) -> Result<Option<i64>, SqlSurfaceError> {
    match v {
        ExprValue::Null => Ok(None),
        ExprValue::Scalar(x) if x.is_finite() && x.fract() == 0.0 && x.abs() <= 2_147_483_648.0 => {
            Ok(Some(*x as i64))
        }
        _ => Err(SqlSurfaceError::Internal {
            detail: "array subscript evaluated to a non-integer value".to_string(),
        }),
    }
}

/// 配列値の取り出し（NULL は `None`）。
pub(super) fn array_of(v: ExprValue<'_>) -> Result<Option<ArrayValue>, SqlSurfaceError> {
    match v {
        ExprValue::Null => Ok(None),
        ExprValue::Array(a) => Ok(Some(a)),
        _ => Err(SqlSurfaceError::Internal {
            detail: "array subscript base evaluated to a non-array value".to_string(),
        }),
    }
}

/// `ARRAY[...]` の評価。要素を順に評価して `elem` の配列へ詰める（NULL 要素は NULL のまま）。
pub(crate) fn eval_array_constructor<'a>(
    elem: ArrayElemType,
    items: &[BoundExpr],
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    if items.len() > MAX_ARRAY_ELEMENTS as usize {
        return Err(SqlSurfaceError::payload_too_large(
            "ARRAY constructor has too many elements",
        ));
    }
    let mut values = Vec::with_capacity(items.len());
    // 要素を所有化（複製）する前にペイロード長を checked 加算し、上限超過で 54000 とする
    // （巨大 TEXT 列を複数並べた構築による過大なメモリ確保を防ぐ）。
    let mut total: usize = 0;
    for item in items {
        let v = crate::sql::udf_call::eval_with_scalars(item, id, embedding, row_scalars)?;
        total = total
            .checked_add(payload_len(&v))
            .filter(|t| *t <= crate::row_codec::MAX_SCALAR_PAYLOAD_LEN as usize)
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large("ARRAY constructor payload is too large")
            })?;
        values.push(own(v)?);
    }
    Ok(ExprValue::Array(values_to_array(values, elem)?))
}

/// `<base>[<index>]` の評価。基底・添字が NULL、または添字が `1..=len` の外なら NULL。
pub(crate) fn eval_array_index<'a>(
    base: &BoundExpr,
    index: &BoundExpr,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    let b = crate::sql::udf_call::eval_with_scalars(base, id, embedding, row_scalars)?;
    let i = crate::sql::udf_call::eval_with_scalars(index, id, embedding, row_scalars)?;
    let (Some(array), Some(i)) = (array_of(b)?, index_value(&i)?) else {
        return Ok(ExprValue::Null);
    };
    // 多次元値への単一添字は添字数の不一致で NULL（PostgreSQL と同じ。多重添字
    // `arr[i][j]` は構文段で `0A000`。Issue #1605）。
    if matches!(array, ArrayValue::Multi(_)) {
        return Ok(ExprValue::Null);
    }
    let len = array.len();
    // `i - 1` は 0 始まりの位置。範囲外は NULL（PG と同じ）。`get()` 相当の `slice` で
    // 1 要素だけを切り出し、添字アクセスを避ける。
    let Some(pos) = i
        .checked_sub(1)
        .and_then(|p| usize::try_from(p).ok())
        .filter(|p| *p < len)
    else {
        return Ok(ExprValue::Null);
    };
    let one = array.slice(pos, pos.saturating_add(1));
    let mut values = array_to_values(&one)?;
    if values.is_empty() {
        return Ok(ExprValue::Null);
    }
    Ok(values.swap_remove(0))
}

/// `<base>[<lo>:<hi>]` の評価。基底または指定された境界が NULL なら NULL。省略した `lo` は 1、
/// `hi` は長さ。範囲は `1..=len` へ切り詰め、空になれば空配列。
pub(crate) fn eval_array_slice<'a>(
    base: &BoundExpr,
    lo: Option<&BoundExpr>,
    hi: Option<&BoundExpr>,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    let b = crate::sql::udf_call::eval_with_scalars(base, id, embedding, row_scalars)?;
    let lo_v = match lo {
        Some(e) => Some(index_value(&crate::sql::udf_call::eval_with_scalars(
            e,
            id,
            embedding,
            row_scalars,
        )?)?),
        None => None,
    };
    let hi_v = match hi {
        Some(e) => Some(index_value(&crate::sql::udf_call::eval_with_scalars(
            e,
            id,
            embedding,
            row_scalars,
        )?)?),
        None => None,
    };
    let Some(array) = array_of(b)? else {
        return Ok(ExprValue::Null);
    };
    // 指定された境界が NULL（`Some(None)`）なら結果は NULL。
    let (lo_v, hi_v) = match (lo_v, hi_v) {
        (Some(None), _) | (_, Some(None)) => return Ok(ExprValue::Null),
        (l, h) => (l.flatten(), h.flatten()),
    };
    // 長さは第 1 次元（多次元値は第 1 次元に沿って切り出す。Issue #1605）。
    let first_dim = array.dims().first().copied().unwrap_or(0);
    let len = i64::from(first_dim);
    let lo_i = lo_v.unwrap_or(1).max(1);
    let hi_i = hi_v.unwrap_or(len).min(len);
    if lo_i > hi_i {
        return Ok(ExprValue::Array(array.slice(0, 0)));
    }
    let start = usize::try_from(lo_i.saturating_sub(1)).unwrap_or(0);
    let end = usize::try_from(hi_i).unwrap_or(0);
    Ok(ExprValue::Array(array.slice(start, end)))
}
