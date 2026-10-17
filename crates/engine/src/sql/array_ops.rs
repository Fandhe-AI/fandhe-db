//! 配列の述語（量化比較 `ANY`／`ALL`・配列どうしの比較・包含 `@>`／`<@`・重なり `&&`）の
//! 束縛と評価（対象ビヘイビア: TABLE-35。Issue #1603 のポインタ）。
//!
//! 責務境界: 構文段（`sql::allowlist::Parser`）が作る [`Expr::Quantified`]・配列の比較
//! （[`Expr::Binary`]）・包含（[`Expr::JsonAccess`] の `Contains`／`ContainedBy`／`Overlap`）を、
//! 束縛段（`sql::udf_call::bind_expr_any`）が本モジュールの `bind_*` で [`BoundExpr`] へ落とす。
//! 評価は `sql::udf_call::eval_with_scalars` が `eval_*` へ委ねる（意味論の実装は 1 つ）。
//! 配列値そのものの生成（`ARRAY[...]`・添字）は `sql::array_expr`、型の変換は `sql::cast` の担当。
//!
//! 要素の比較は [`Key`]（型付きの借用ビュー）に一本化し、比較・包含・量化で共有する。bigint を
//! f64 に落とさず正確に比較し、浮動小数は PG と同じく NaN を最大かつ自身と等しいとして全順序にする。
//! numeric・json（原文保存）・ENUM の要素は比較器を持たないため、束縛段で `0A000`
//! （黙って別の意味で評価しない。fail-closed）。
//! 包含・重なりは片側を整列して二分探索する O((n+m) log n) で、要素数上限（1024）での
//! 二乗の比較を避ける。

use std::cmp::Ordering;

use crate::catalog::ArrayElemType;
use crate::row_codec::{ArrayValue, ScalarRef};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::cast::{
    bind_cast, bind_operand, elem_cast_type, source_cast_kind, CastKind, CastType, SourceKind,
};
use crate::sql::udf_call::{
    is_array_expr, ArrayCmpOp, ArrayContainOp, BinOp, BindEnv, BoundExpr, Expr, ExprType,
    ExprValue, JsonAccessOp, Quantifier,
};

/// 要素 1 個の比較用ビュー。整数系（integer・bigint・date・timestamp）は `Int`、浮動小数
/// （real・double）は `Float`。
#[derive(Clone, Copy, Debug)]
pub(crate) enum Key<'a> {
    Int(i64),
    Float(f64),
    Text(&'a str),
    Bool(bool),
    Bytes(&'a [u8]),
    Uuid(crate::uuid::Uuid),
    /// jsonb の正規化テキスト。全順序は等値と整合するバイト列順だけ（意味順は持たない）で、
    /// 順序比較の演算子は束縛段が拒否する。
    Json(&'a str),
}

impl Key<'_> {
    /// 異なる型の組を決定的に並べるための序数（束縛段が同じ型の組だけを通すため通常は使われない）。
    fn rank(&self) -> u8 {
        match self {
            Key::Int(_) | Key::Float(_) => 0,
            Key::Text(_) => 1,
            Key::Bool(_) => 2,
            Key::Bytes(_) => 3,
            Key::Uuid(_) => 4,
            Key::Json(_) => 5,
        }
    }
}

/// PG の float8 の順序（NaN は最大で NaN どうしは等しい。`-0.0` と `0.0` は等しい）。
fn float_cmp(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// 整数と浮動小数の正確な比較（量化比較の左辺が数値、要素が整数のとき）。
fn int_float_cmp(i: i64, f: f64) -> Ordering {
    if f.is_nan() {
        return Ordering::Less;
    }
    // i64 の範囲外の浮動小数は整数より必ず大きい（小さい）。
    if f >= 9_223_372_036_854_775_808.0 {
        return Ordering::Less;
    }
    if f < -9_223_372_036_854_775_808.0 {
        return Ordering::Greater;
    }
    let t = f.trunc();
    let ti = t as i64;
    match i.cmp(&ti) {
        Ordering::Equal => {
            let frac = f - t;
            if frac > 0.0 {
                Ordering::Less
            } else if frac < 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        other => other,
    }
}

/// 要素の全順序。`=` と矛盾しない（等しいと判定される組は常に `Equal`）。
pub(crate) fn key_cmp(a: &Key<'_>, b: &Key<'_>) -> Ordering {
    match (a, b) {
        (Key::Int(x), Key::Int(y)) => x.cmp(y),
        (Key::Float(x), Key::Float(y)) => float_cmp(*x, *y),
        (Key::Int(x), Key::Float(y)) => int_float_cmp(*x, *y),
        (Key::Float(x), Key::Int(y)) => int_float_cmp(*y, *x).reverse(),
        (Key::Text(x), Key::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Key::Bool(x), Key::Bool(y)) => x.cmp(y),
        (Key::Bytes(x), Key::Bytes(y)) => x.cmp(y),
        (Key::Uuid(x), Key::Uuid(y)) => x.cmp(y),
        (Key::Json(x), Key::Json(y)) => x.as_bytes().cmp(y.as_bytes()),
        _ => a.rank().cmp(&b.rank()),
    }
}

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

fn unsupported(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: detail.to_string(),
    }
}

fn operator_missing(op: &str) -> SqlSurfaceError {
    SqlSurfaceError::undefined_function(format!(
        "operator does not exist: the operand types do not match the {op} operator"
    ))
}

/// 配列値を要素ごとの比較用ビューへ写す（NULL 要素は `None`）。比較器を持たない要素型は
/// 束縛段が拒否済みのため、ここへ来たら内部エラー。
pub(crate) fn array_keys(a: &ArrayValue) -> Result<Vec<Option<Key<'_>>>, SqlSurfaceError> {
    fn map<'a, T>(items: &'a [Option<T>], f: impl Fn(&'a T) -> Key<'a>) -> Vec<Option<Key<'a>>> {
        items.iter().map(|e| e.as_ref().map(&f)).collect()
    }
    Ok(match a {
        ArrayValue::Text(v) => map(v, |s| Key::Text(s.as_str())),
        ArrayValue::Bool(v) => map(v, |b| Key::Bool(*b)),
        ArrayValue::Integer(v) => map(v, |i| Key::Int(i64::from(*i))),
        ArrayValue::BigInt(v) => map(v, |i| Key::Int(*i)),
        ArrayValue::Real(v) => map(v, |x| Key::Float(f64::from(*x))),
        ArrayValue::Double(v) => map(v, |x| Key::Float(*x)),
        ArrayValue::Date(v) => map(v, |d| Key::Int(i64::from(*d))),
        ArrayValue::Timestamp(v) => map(v, |t| Key::Int(*t)),
        ArrayValue::Uuid(v) => map(v, |u| Key::Uuid(*u)),
        ArrayValue::Bytea(v) => map(v, |b| Key::Bytes(b.as_slice())),
        ArrayValue::Jsonb(v) => map(v, |s| Key::Json(s.as_str())),
        ArrayValue::Numeric(_) | ArrayValue::Json(_) | ArrayValue::Enum(_) => {
            return Err(internal("array element type has no comparator"))
        }
    })
}

/// 比較演算子が順序 `ord` で成り立つか。
fn op_holds(op: ArrayCmpOp, ord: Ordering) -> bool {
    match op {
        ArrayCmpOp::Eq => ord == Ordering::Equal,
        ArrayCmpOp::Ne => ord != Ordering::Equal,
        ArrayCmpOp::Lt => ord == Ordering::Less,
        ArrayCmpOp::Le => ord != Ordering::Greater,
        ArrayCmpOp::Gt => ord == Ordering::Greater,
        ArrayCmpOp::Ge => ord != Ordering::Less,
    }
}

/// 配列どうしの全順序（`array_cmp` 相当）。先頭から要素を比べ、NULL 要素は非 NULL より大きく
/// NULL どうしは等しい。共通部分がすべて等しければ短い方が小さい。`=` は長さ一致かつ全要素等しい
/// ことと同値で、この順序の `Equal` と一致する。
pub(crate) fn compare_arrays(a: &[Option<Key<'_>>], b: &[Option<Key<'_>>]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = match (x, y) {
            (Some(x), Some(y)) => key_cmp(x, y),
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// `container` の非 NULL 要素を整列した列（包含・重なりの二分探索用）。
fn sorted_non_null<'a>(items: &[Option<Key<'a>>]) -> Vec<Key<'a>> {
    let mut v: Vec<Key<'a>> = items.iter().flatten().copied().collect();
    v.sort_by(key_cmp);
    v
}

fn contains_key(sorted: &[Key<'_>], needle: &Key<'_>) -> bool {
    sorted.binary_search_by(|k| key_cmp(k, needle)).is_ok()
}

/// `container @> items`。`items` の非 NULL 要素がすべて `container` に在れば真。NULL 要素は
/// どの要素とも一致しないので `items` に 1 つでもあれば偽。空の `items` は真。
pub(crate) fn array_contains(container: &[Option<Key<'_>>], items: &[Option<Key<'_>>]) -> bool {
    if items.iter().any(Option::is_none) {
        return false;
    }
    let sorted = sorted_non_null(container);
    items.iter().flatten().all(|k| contains_key(&sorted, k))
}

/// `a && b`。共通する非 NULL 要素が 1 つでもあれば真（NULL 要素は無視。空配列との重なりは偽）。
pub(crate) fn array_overlaps(a: &[Option<Key<'_>>], b: &[Option<Key<'_>>]) -> bool {
    // 小さい方を整列して探索する。
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let sorted = sorted_non_null(small);
    large.iter().flatten().any(|k| contains_key(&sorted, k))
}

// ---------------------------------------------------------------- 束縛

/// 要素型が比較器を持つか（numeric・json・ENUM は持たない）。
fn comparable_elem(elem: ArrayElemType) -> bool {
    !matches!(
        elem,
        ArrayElemType::Numeric { .. } | ArrayElemType::Json | ArrayElemType::Enum
    )
}

/// 配列式のオペランドを束縛して要素型を得る。配列でない式・要素型を確定できない配列は拒否する。
fn bind_array_operand(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ArrayElemType), SqlSurfaceError> {
    let (bound, ty) = bind_operand(expr, env, node_budget)?;
    if ty != ExprType::Array {
        return Err(operator_missing("array"));
    }
    let elem = match source_cast_kind(expr, &bound, ty, env.schema()) {
        SourceKind::Array(Some(t)) => CastType::Array(Box::new(t)).array_elem_type(),
        _ => None,
    };
    let Some(elem) = elem else {
        return Err(unsupported(
            "array predicates on an array of this element type are not supported yet",
        ));
    };
    if !comparable_elem(elem) {
        return Err(unsupported(
            "array predicates on numeric, json or enum elements are not supported yet",
        ));
    }
    Ok((bound, elem))
}

/// 型なしリテラル側（文字列リテラル・`NULL`）か。反対側の配列型として解釈する対象。
fn is_untyped_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::String(_) | Expr::Null)
}

/// 配列どうしの 2 項述語（比較・包含・重なり）のオペランド対を束縛し、共通の要素型を返す。
/// どちらか一方が配列式で、他方が配列式または型なしリテラルである組だけを受理する。
/// 要素型が異なる配列どうしは `42883`（PG の `anyarray` 演算子は両辺が同じ型）。
fn bind_array_pair(
    op_sql: &str,
    lhs: &Expr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, BoundExpr, ArrayElemType), SqlSurfaceError> {
    if crate::sql::udf_call::in_check_binding() {
        return Err(unsupported(
            "array operators are not supported in a CHECK constraint",
        ));
    }
    let l_arr = is_array_expr(lhs, env);
    let r_arr = is_array_expr(rhs, env);
    let literal_as = |elem: ArrayElemType| -> Result<CastType, SqlSurfaceError> {
        elem_cast_type(elem)
            .map(|t| CastType::Array(Box::new(t)))
            .ok_or_else(|| unsupported("array predicates on this element type are not supported"))
    };
    match (l_arr, r_arr) {
        (true, true) => {
            let (l, le) = bind_array_operand(lhs, env, node_budget)?;
            let (r, re) = bind_array_operand(rhs, env, node_budget)?;
            if le != re {
                return Err(operator_missing(op_sql));
            }
            Ok((l, r, le))
        }
        (true, false) if is_untyped_literal(rhs) => {
            let (l, le) = bind_array_operand(lhs, env, node_budget)?;
            let (r, _) = bind_cast(rhs, &literal_as(le)?, env, node_budget)?;
            Ok((l, r, le))
        }
        (false, true) if is_untyped_literal(lhs) => {
            let (r, re) = bind_array_operand(rhs, env, node_budget)?;
            let (l, _) = bind_cast(lhs, &literal_as(re)?, env, node_budget)?;
            Ok((l, r, re))
        }
        _ => Err(operator_missing(op_sql)),
    }
}

/// 配列どうしの比較 `=` `<` `<=` `>` `>=` の束縛（`udf_call::bind_expr_any` の比較演算子の腕から、
/// どちらかのオペランドが配列式のときに呼ばれる。呼び出し側が演算ノード自身を課金済み）。
pub(super) fn bind_array_comparison(
    op: BinOp,
    lhs: &Expr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let (cmp, sql) = match op {
        BinOp::Eq => (ArrayCmpOp::Eq, "="),
        BinOp::Lt => (ArrayCmpOp::Lt, "<"),
        BinOp::Le => (ArrayCmpOp::Le, "<="),
        BinOp::Gt => (ArrayCmpOp::Gt, ">"),
        BinOp::Ge => (ArrayCmpOp::Ge, ">="),
        _ => return Err(operator_missing("array")),
    };
    let (l, r, elem) = bind_array_pair(sql, lhs, rhs, env, node_budget)?;
    reject_json_ordering(cmp, elem)?;
    Ok((
        BoundExpr::ArrayCompare {
            op: cmp,
            elem,
            lhs: Box::new(l),
            rhs: Box::new(r),
        },
        ExprType::Bool,
    ))
}

/// jsonb 要素は等値だけを持つ（意味順を実装していない）。順序比較は `0A000`。
fn reject_json_ordering(op: ArrayCmpOp, elem: ArrayElemType) -> Result<(), SqlSurfaceError> {
    if elem == ArrayElemType::Jsonb && !matches!(op, ArrayCmpOp::Eq | ArrayCmpOp::Ne) {
        return Err(unsupported(
            "ordering comparison of jsonb array elements is not supported yet",
        ));
    }
    Ok(())
}

/// 配列の `@>` `<@` `&&` の束縛（`bind_json_access` から、配列オペランドを持つ形のときに呼ばれる）。
pub(super) fn bind_array_containment(
    op: JsonAccessOp,
    lhs: &Expr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let cop = match op {
        JsonAccessOp::Contains => ArrayContainOp::Contains,
        JsonAccessOp::ContainedBy => ArrayContainOp::ContainedBy,
        JsonAccessOp::Overlap => ArrayContainOp::Overlap,
        _ => {
            return Err(internal(
                "non-containment operator reached the array binder",
            ))
        }
    };
    let (l, r, elem) = bind_array_pair(op.as_sql(), lhs, rhs, env, node_budget)?;
    Ok((
        BoundExpr::ArrayContainment {
            op: cop,
            elem,
            lhs: Box::new(l),
            rhs: Box::new(r),
        },
        ExprType::Bool,
    ))
}

/// 左辺の型が量化比較の要素型と比較できるか。数値は整数・浮動小数の要素どうし、他は同じ型どうし。
/// date と timestamp の混在は PG では通るが比較器が無いため `0A000`、それ以外の不一致は `42883`。
fn check_lhs_compatible(kind: CastKind, elem: ArrayElemType) -> Result<(), SqlSurfaceError> {
    use ArrayElemType as E;
    use CastKind as K;
    let ok = match kind {
        K::Int2 | K::Int4 | K::Int8 | K::Float4 | K::Float8 => {
            matches!(elem, E::Integer | E::BigInt | E::Real | E::Double)
        }
        K::Text | K::Varchar | K::Bpchar => elem == E::Text,
        K::Bool => elem == E::Bool,
        K::Date => elem == E::Date,
        K::Timestamp => elem == E::Timestamp,
        K::Uuid => elem == E::Uuid,
        K::Bytea => elem == E::Bytea,
        K::Jsonb => elem == E::Jsonb,
        K::Numeric => {
            return Err(unsupported(
                "ANY/ALL with a numeric operand is not supported yet",
            ))
        }
        _ => false,
    };
    if ok {
        return Ok(());
    }
    if matches!(
        (kind, elem),
        (K::Date, E::Timestamp) | (K::Timestamp, E::Date)
    ) {
        return Err(unsupported(
            "comparing date with timestamp array elements is not supported yet",
        ));
    }
    Err(operator_missing("ANY/ALL"))
}

/// 左辺の種別から、型なしリテラルの配列を解釈する要素の CAST 型を決める。
fn lhs_elem_cast_type(kind: CastKind) -> Result<CastType, SqlSurfaceError> {
    Ok(match kind {
        CastKind::Int4 => CastType::Int4,
        CastKind::Int8 => CastType::Int8,
        CastKind::Float4 => CastType::Float4,
        CastKind::Float8 => CastType::Float8,
        CastKind::Text | CastKind::Varchar | CastKind::Bpchar => CastType::Text,
        CastKind::Bool => CastType::Bool,
        CastKind::Date => CastType::Date,
        CastKind::Timestamp => CastType::Timestamp,
        CastKind::Uuid => CastType::Uuid,
        CastKind::Bytea => CastType::Bytea,
        CastKind::Jsonb => CastType::Jsonb,
        _ => {
            return Err(unsupported(
                "the array type of this ANY/ALL operand cannot be determined",
            ))
        }
    })
}

/// 量化比較 `lhs op ANY|ALL (rhs)` の束縛（`udf_call::bind_expr_any` の `Expr::Quantified` 腕から呼ばれる。
/// 呼び出し側が構築ノード自身を課金済み）。右辺は配列式または型なしリテラル（左辺の型の配列として
/// 解釈）に限り、配列でない右辺は PG と同じく `42809` 相当ではなく型不一致 `42804` とする。
pub(super) fn bind_quantified(
    op: ArrayCmpOp,
    quantifier: Quantifier,
    lhs: &Expr,
    rhs: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if crate::sql::udf_call::in_check_binding() {
        return Err(unsupported(
            "ANY/ALL is not supported in a CHECK constraint",
        ));
    }
    let r_arr = is_array_expr(rhs, env);
    if !r_arr && !is_untyped_literal(rhs) {
        return Err(SqlSurfaceError::datatype_mismatch(
            "op ANY/ALL (array) requires array on right side",
        ));
    }
    let (l_bound, r_bound, elem) = if r_arr {
        let (array, elem) = bind_array_operand(rhs, env, node_budget)?;
        let (l, _) = bind_quantified_lhs(lhs, Some(elem), env, node_budget)?;
        (l, array, elem)
    } else {
        // 右辺が型なしリテラル: 左辺の型の配列として解釈する（左辺もリテラルなら text）。
        let (l, kind) = bind_quantified_lhs(lhs, None, env, node_budget)?;
        let target = CastType::Array(Box::new(lhs_elem_cast_type(kind)?));
        let (array, _) = bind_cast(rhs, &target, env, node_budget)?;
        let elem = target
            .array_elem_type()
            .ok_or_else(|| unsupported("ANY/ALL on this element type is not supported yet"))?;
        if !comparable_elem(elem) {
            return Err(unsupported(
                "ANY/ALL on numeric, json or enum elements is not supported yet",
            ));
        }
        check_lhs_compatible(kind, elem)?;
        (l, array, elem)
    };
    reject_json_ordering(op, elem)?;
    Ok((
        BoundExpr::ArrayQuantified {
            op,
            all: quantifier == Quantifier::All,
            elem,
            lhs: Box::new(l_bound),
            array: Box::new(r_bound),
        },
        ExprType::Bool,
    ))
}

/// 量化比較の左辺を束縛し、種別を返す。`elem` があれば（配列側が確定済み）型なしリテラルをその要素型へ
/// 解釈し、非リテラルは要素型との適合を検査する。`elem` が無い（配列側がリテラル）ときは、リテラルの左辺を
/// text として扱う。
fn bind_quantified_lhs(
    lhs: &Expr,
    elem: Option<ArrayElemType>,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, CastKind), SqlSurfaceError> {
    if is_untyped_literal(lhs) {
        let target = match elem {
            Some(e) => elem_cast_type(e)
                .ok_or_else(|| unsupported("ANY/ALL on this element type is not supported yet"))?,
            None => CastType::Text,
        };
        let (bound, _) = bind_cast(lhs, &target, env, node_budget)?;
        return Ok((bound, target.kind()));
    }
    // 小数形の数値リテラルを浮動小数の要素と比べるときは float8 同士の比較（PG の numeric = float8 は
    // float8 へ寄せる）。f64 を経由しない numeric のまま比べる形は未対応（`0A000`）。
    if matches!(lhs, Expr::Number(_))
        && matches!(elem, Some(ArrayElemType::Real | ArrayElemType::Double))
    {
        let (bound, _) = bind_cast(lhs, &CastType::Float8, env, node_budget)?;
        return Ok((bound, CastKind::Float8));
    }
    let (bound, ty) = bind_operand(lhs, env, node_budget)?;
    let kind = match source_cast_kind(lhs, &bound, ty, env.schema()) {
        SourceKind::Known(k) => k,
        // 種別を静的に決められない数値は float8 と同じ比較（整数・浮動小数の要素どうし）。
        SourceKind::UnknownNumeric => CastKind::Float8,
        SourceKind::Array(_) | SourceKind::Outside => return Err(operator_missing("ANY/ALL")),
    };
    if let Some(e) = elem {
        check_lhs_compatible(kind, e)?;
    }
    Ok((bound, kind))
}

// ---------------------------------------------------------------- 評価

/// 評価値から配列を取り出す（NULL は `None`）。
fn expect_array(v: ExprValue<'_>) -> Result<Option<ArrayValue>, SqlSurfaceError> {
    match v {
        ExprValue::Null => Ok(None),
        ExprValue::Array(a) => Ok(Some(a)),
        _ => Err(internal(
            "array predicate operand evaluated to a non-array value",
        )),
    }
}

/// 配列どうしの比較の評価。どちらかの配列が NULL なら NULL。
pub(crate) fn eval_array_compare<'a>(
    op: ArrayCmpOp,
    lhs: &BoundExpr,
    rhs: &BoundExpr,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    let l = crate::sql::udf_call::eval_with_scalars(lhs, id, embedding, row_scalars)?;
    let r = crate::sql::udf_call::eval_with_scalars(rhs, id, embedding, row_scalars)?;
    let (Some(l), Some(r)) = (expect_array(l)?, expect_array(r)?) else {
        return Ok(ExprValue::Null);
    };
    let ord = compare_arrays(&array_keys(&l)?, &array_keys(&r)?);
    Ok(ExprValue::Bool(op_holds(op, ord)))
}

/// 配列の包含・被包含・重なりの評価。どちらかの配列が NULL なら NULL。
pub(crate) fn eval_array_containment<'a>(
    op: ArrayContainOp,
    lhs: &BoundExpr,
    rhs: &BoundExpr,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    let l = crate::sql::udf_call::eval_with_scalars(lhs, id, embedding, row_scalars)?;
    let r = crate::sql::udf_call::eval_with_scalars(rhs, id, embedding, row_scalars)?;
    let (Some(l), Some(r)) = (expect_array(l)?, expect_array(r)?) else {
        return Ok(ExprValue::Null);
    };
    let (lk, rk) = (array_keys(&l)?, array_keys(&r)?);
    let result = match op {
        ArrayContainOp::Contains => array_contains(&lk, &rk),
        ArrayContainOp::ContainedBy => array_contains(&rk, &lk),
        ArrayContainOp::Overlap => array_overlaps(&lk, &rk),
    };
    Ok(ExprValue::Bool(result))
}

/// 左辺の評価値を比較用ビューへ（NULL は `None`）。
fn value_key<'v>(v: &'v ExprValue<'_>) -> Result<Option<Key<'v>>, SqlSurfaceError> {
    Ok(Some(match v {
        ExprValue::Null => return Ok(None),
        ExprValue::Scalar(x) => Key::Float(*x),
        ExprValue::Text(s) => Key::Text(s.as_ref()),
        ExprValue::Bool(b) => Key::Bool(*b),
        ExprValue::Date(d) => Key::Int(i64::from(*d)),
        ExprValue::Timestamp(t) => Key::Int(*t),
        ExprValue::Uuid(u) => Key::Uuid(*u),
        ExprValue::Bytes(b) => Key::Bytes(b.as_ref()),
        ExprValue::Json(s) => Key::Json(s.as_ref()),
        _ => {
            return Err(internal(
                "ANY/ALL operand evaluated to an unsupported value",
            ))
        }
    }))
}

/// 量化比較の純粋な三値論理。`lhs` が `None` なら各比較は不明。空の配列は左辺が NULL でも
/// ANY が偽・ALL が真（比較を 1 度も評価しないため）。戻り値の `None` は SQL の NULL。
pub(crate) fn quantify(
    op: ArrayCmpOp,
    all: bool,
    lhs: Option<&Key<'_>>,
    items: &[Option<Key<'_>>],
) -> Option<bool> {
    let mut saw_unknown = false;
    for item in items {
        let outcome = match (lhs, item) {
            (Some(l), Some(r)) => Some(op_holds(op, key_cmp(l, r))),
            _ => None,
        };
        match outcome {
            Some(true) if !all => return Some(true),
            Some(false) if all => return Some(false),
            Some(_) => {}
            None => saw_unknown = true,
        }
    }
    if saw_unknown {
        None
    } else {
        Some(all)
    }
}

/// 量化比較の評価。配列が NULL なら NULL（空かどうかの判定より先）。
pub(crate) fn eval_array_quantified<'a>(
    op: ArrayCmpOp,
    all: bool,
    lhs: &BoundExpr,
    array: &BoundExpr,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    let a = crate::sql::udf_call::eval_with_scalars(array, id, embedding, row_scalars)?;
    let Some(a) = expect_array(a)? else {
        return Ok(ExprValue::Null);
    };
    let items = array_keys(&a)?;
    let l = crate::sql::udf_call::eval_with_scalars(lhs, id, embedding, row_scalars)?;
    let lkey = value_key(&l)?;
    Ok(match quantify(op, all, lkey.as_ref(), &items) {
        Some(b) => ExprValue::Bool(b),
        None => ExprValue::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(v: &[Option<i64>]) -> Vec<Option<Key<'static>>> {
        v.iter().map(|x| x.map(Key::Int)).collect()
    }

    /// TABLE-35・Issue #1603: 配列の全順序（NULL 要素は最大・短い方が小さい・NULL どうしは等しい）。
    #[test]
    fn compare_arrays_orders_like_postgres() {
        let a = ints(&[Some(1), Some(2)]);
        let b = ints(&[Some(1), Some(3)]);
        assert_eq!(compare_arrays(&a, &b), Ordering::Less);
        assert_eq!(
            compare_arrays(&ints(&[Some(1)]), &ints(&[Some(1), Some(0)])),
            Ordering::Less
        );
        assert_eq!(
            compare_arrays(&ints(&[Some(1), None]), &ints(&[Some(1), Some(9)])),
            Ordering::Greater
        );
        assert_eq!(
            compare_arrays(&ints(&[None, Some(1)]), &ints(&[None, Some(1)])),
            Ordering::Equal
        );
        assert_eq!(compare_arrays(&[], &[]), Ordering::Equal);
    }

    /// 浮動小数は NaN が最大で自身と等しい。整数との比較は桁あふれなく正確。
    #[test]
    fn float_and_mixed_comparisons_are_total() {
        assert_eq!(
            key_cmp(&Key::Float(f64::NAN), &Key::Float(f64::NAN)),
            Ordering::Equal
        );
        assert_eq!(
            key_cmp(&Key::Float(f64::NAN), &Key::Float(1e300)),
            Ordering::Greater
        );
        assert_eq!(
            key_cmp(&Key::Float(-0.0), &Key::Float(0.0)),
            Ordering::Equal
        );
        assert_eq!(key_cmp(&Key::Int(2), &Key::Float(2.0)), Ordering::Equal);
        assert_eq!(key_cmp(&Key::Int(2), &Key::Float(2.5)), Ordering::Less);
        assert_eq!(key_cmp(&Key::Float(2.5), &Key::Int(2)), Ordering::Greater);
        assert_eq!(
            key_cmp(
                &Key::Int(9_007_199_254_740_993),
                &Key::Float(9_007_199_254_740_992.0)
            ),
            Ordering::Greater
        );
        assert_eq!(
            key_cmp(&Key::Int(i64::MAX), &Key::Float(1e30)),
            Ordering::Less
        );
        assert_eq!(
            key_cmp(&Key::Int(i64::MIN), &Key::Float(-1e30)),
            Ordering::Greater
        );
    }

    /// `@>`・`<@`・`&&` の NULL 要素・空配列・重複の扱い。
    #[test]
    fn containment_and_overlap_follow_null_rules() {
        let abc = ints(&[Some(1), Some(2), Some(3)]);
        assert!(array_contains(&abc, &ints(&[Some(3), Some(1), Some(1)])));
        assert!(!array_contains(&abc, &ints(&[Some(4)])));
        // 空配列は任意の配列に含まれる。
        assert!(array_contains(&abc, &[]));
        assert!(array_contains(&[], &[]));
        // NULL 要素はどの要素とも一致しない（NULL どうしでも）。
        assert!(!array_contains(&ints(&[None]), &ints(&[None])));
        assert!(!array_contains(&abc, &ints(&[Some(1), None])));
        assert!(array_contains(&ints(&[Some(1), None]), &ints(&[Some(1)])));
        // 重なり: NULL は無視、空配列との重なりは偽。
        assert!(array_overlaps(&abc, &ints(&[Some(9), Some(2)])));
        assert!(!array_overlaps(&abc, &ints(&[Some(9)])));
        assert!(!array_overlaps(&abc, &[]));
        assert!(!array_overlaps(&ints(&[None]), &ints(&[None])));
    }

    /// 量化比較の三値論理と空配列の規則（左辺が NULL でも ANY は偽・ALL は真）。
    #[test]
    fn quantify_follows_three_valued_logic() {
        let one = Key::Int(1);
        let arr = ints(&[Some(1), Some(2)]);
        assert_eq!(
            quantify(ArrayCmpOp::Eq, false, Some(&one), &arr),
            Some(true)
        );
        assert_eq!(
            quantify(ArrayCmpOp::Eq, true, Some(&one), &arr),
            Some(false)
        );
        assert_eq!(
            quantify(ArrayCmpOp::Ne, true, Some(&one), &arr),
            Some(false)
        );
        assert_eq!(
            quantify(ArrayCmpOp::Lt, true, Some(&Key::Int(0)), &arr),
            Some(true)
        );
        // NULL 要素: 真になる比較が無ければ NULL、真があれば真。
        let with_null = ints(&[Some(5), None]);
        assert_eq!(
            quantify(ArrayCmpOp::Eq, false, Some(&one), &with_null),
            None
        );
        assert_eq!(
            quantify(ArrayCmpOp::Eq, false, Some(&Key::Int(5)), &with_null),
            Some(true)
        );
        // ALL: 偽があれば偽、不明があれば NULL。
        assert_eq!(
            quantify(ArrayCmpOp::Eq, true, Some(&one), &with_null),
            Some(false)
        );
        assert_eq!(
            quantify(ArrayCmpOp::Gt, true, Some(&Key::Int(9)), &with_null),
            None
        );
        // 左辺 NULL。
        assert_eq!(quantify(ArrayCmpOp::Eq, false, None, &arr), None);
        assert_eq!(quantify(ArrayCmpOp::Eq, false, None, &[]), Some(false));
        assert_eq!(quantify(ArrayCmpOp::Eq, true, None, &[]), Some(true));
    }

    /// 要素数上限（1024）どうしの包含・重なりは整列＋二分探索で完了し、結果も正しい。
    #[test]
    fn large_arrays_complete_with_log_linear_work() {
        let a: Vec<Option<Key<'static>>> = (0..1024i64).map(|i| Some(Key::Int(i))).collect();
        let b: Vec<Option<Key<'static>>> = (0..1024i64).rev().map(|i| Some(Key::Int(i))).collect();
        assert!(array_contains(&a, &b));
        assert!(array_overlaps(&a, &b));
        let c: Vec<Option<Key<'static>>> = (1024..2048i64).map(|i| Some(Key::Int(i))).collect();
        assert!(!array_overlaps(&a, &c));
        assert!(!array_contains(&a, &c));
    }

    /// `=` と全順序の整合（否定の押し下げが `a < b OR a > b` に書き換えても漏れない）。
    #[test]
    fn equality_agrees_with_total_order() {
        let a = ints(&[Some(5), None, Some(7)]);
        let b = ints(&[Some(5), None, Some(7)]);
        assert_eq!(compare_arrays(&a, &b), Ordering::Equal);
        let c = ints(&[Some(5), Some(1), Some(7)]);
        assert_ne!(compare_arrays(&a, &c), Ordering::Equal);
    }
}
