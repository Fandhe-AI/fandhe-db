//! 配列関数（`array_length`／`cardinality`／`array_append`／`array_prepend`／`array_cat`／
//! `array_position`／`array_remove`／`array_to_string`）の束縛と評価
//! （対象ビヘイビア: TABLE-35。Issue #1604 のポインタ）。
//!
//! 責務境界: `sql::udf_call::bind_call` が名前を [`is_array_fn_name`] で判定して
//! [`bind_array_fn`] へ委譲し、式評価器（`eval_with_scalars`）が束縛結果
//! （`BoundExpr::ArrayFn`）を [`eval_array_fn`] で評価する。`ARRAY[...]`・添字は
//! `sql::array_expr`、要素の型変換は `sql::cast` に委ね、本モジュールは「関数ごとの引数の
//! 型解決（多相の要素型の決定）」と「NULL・空配列の意味論」だけを持つ。
//!
//! 固定アリティ・静的型の `BuiltinFn` には収まらない（多相の要素型・strict でない NULL 規則）
//! ため別の束縛種別とした。要素型の共通化は `array_expr::common_element_type` を共有する
//! （第 2 の共通型実装を作らない）。連結・検索・除去・文字列化は `ArrayValue` の native 値で行い、
//! 式評価の f64 表現を経由しない（2^53 超の bigint 要素でも `22003` にならない）。
//!
//! 安全側の設計: 要素型を決められない配列（ENUM 要素）・numeric 要素・等価の無い `json` 要素の
//! 検索は拒否する。結果の要素数は `MAX_ARRAY_ELEMENTS`、可変長本文の累計は
//! `MAX_SCALAR_PAYLOAD_LEN`、文字列化の出力は `MAX_TEXT_FIELD_LEN` を、確保前に検査して
//! 超過を `54000` とする。エラー文に要素値・リテラル本文は含めない。

use std::borrow::Cow;

use crate::catalog::{ArrayElemType, MAX_ARRAY_ELEMENTS};
use crate::row_codec::{ArrayValue, ScalarRef, MAX_SCALAR_PAYLOAD_LEN, MAX_TEXT_FIELD_LEN};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::array_expr::{
    array_of, bind_subscript_index, classify_item, common_element_type, index_value, own,
    unsupported, ItemClass,
};
use crate::sql::cast::{
    bind_cast, bind_operand, cast_bound, charge_node, source_cast_kind, CastType, SourceKind,
};
use crate::sql::cast_value::values_to_array;
use crate::sql::udf_call::{
    bind_expr_any, eval_with_scalars, BindEnv, BoundExpr, Expr, ExprType, ExprValue,
};

/// 配列関数の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArrayFnKind {
    Length,
    Cardinality,
    Append,
    Prepend,
    Cat,
    Position,
    Remove,
    ToString,
}

/// 配列関数の束縛結果（`BoundExpr::ArrayFn` が保持する）。
///
/// `args` は呼び出しの引数順ではなく正準順（配列, 要素／区切り, 追加引数）。`array_prepend` も
/// 配列が先頭で、評価器は種別だけで意味を決める。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundArrayFn {
    pub(crate) kind: ArrayFnKind,
    /// 要素を扱う関数（append／prepend／cat／position／remove）の共通の要素型。それ以外は `None`。
    pub(crate) elem: Option<ArrayElemType>,
    pub(crate) args: Vec<BoundExpr>,
}

impl BoundArrayFn {
    /// 結果が int4 か（`array_length`・`cardinality`・`array_position`。結果列の型 OID を 23 にする）。
    pub(crate) fn result_is_int4(&self) -> bool {
        matches!(
            self.kind,
            ArrayFnKind::Length | ArrayFnKind::Cardinality | ArrayFnKind::Position
        )
    }
}

/// 本モジュールが扱う関数名（小文字）か。
pub(crate) fn is_array_fn_name(lower: &str) -> bool {
    matches!(
        lower,
        "array_length"
            | "cardinality"
            | "array_append"
            | "array_prepend"
            | "array_cat"
            | "array_position"
            | "array_remove"
            | "array_to_string"
    )
}

fn arity_error(name: &str, expected: &str, got: usize) -> SqlSurfaceError {
    SqlSurfaceError::undefined_function(format!(
        "function {name} expects {expected} argument(s), got {got}"
    ))
}

fn no_such_function(name: &str) -> SqlSurfaceError {
    SqlSurfaceError::undefined_function(format!(
        "function {name} does not exist for the given argument types"
    ))
}

fn polymorphic_unresolved() -> SqlSurfaceError {
    SqlSurfaceError::datatype_mismatch(
        "could not determine polymorphic type because input has type unknown",
    )
}

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

/// 束縛済みの配列引数。裸の `NULL` は型を持たない（相手の引数から型が決まる関数だけが許す）。
enum ArrArg<'e> {
    Null,
    Array {
        expr: &'e Expr,
        bound: BoundExpr,
        ty: ExprType,
        elem: CastType,
    },
}

impl ArrArg<'_> {
    fn elem(&self) -> Option<&CastType> {
        match self {
            ArrArg::Null => None,
            ArrArg::Array { elem, .. } => Some(elem),
        }
    }
}

/// 配列を要求する引数位置の束縛。型なし文字列リテラルは PG と同じく多相型を決められず `42804`、
/// 配列でない型は `42883`、要素型を表せない配列（ENUM 要素）は `0A000`。
fn bind_array_arg<'e>(
    name: &str,
    expr: &'e Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<ArrArg<'e>, SqlSurfaceError> {
    match expr {
        Expr::Null => {
            charge_node(node_budget)?;
            return Ok(ArrArg::Null);
        }
        Expr::String(_) => return Err(polymorphic_unresolved()),
        _ => {}
    }
    let (bound, ty) = bind_operand(expr, env, node_budget)?;
    match source_cast_kind(expr, &bound, ty, env.schema()) {
        SourceKind::Array(Some(elem)) => Ok(ArrArg::Array {
            expr,
            bound,
            ty,
            elem,
        }),
        SourceKind::Array(None) => Err(unsupported(
            "an array function on an array of this element type is not supported",
        )),
        _ => Err(no_such_function(name)),
    }
}

/// 配列引数が `NULL` 単独でなく配列であることを要求して、束縛式を取り出す。
fn require_array(arg: ArrArg<'_>) -> Result<BoundExpr, SqlSurfaceError> {
    match arg {
        ArrArg::Null => Err(polymorphic_unresolved()),
        ArrArg::Array { bound, .. } => Ok(bound),
    }
}

/// 配列の要素型（と要素引数の種別）から、結果・比較に使う共通の要素型を決める。
/// 共通型の無い組は PG の関数解決に合わせて `42883`。
fn resolve_target(
    name: &str,
    arr_elems: &[&CastType],
    class: Option<&ItemClass>,
) -> Result<(CastType, ArrayElemType), SqlSurfaceError> {
    let kinds: Vec<_> = arr_elems
        .iter()
        .map(|t| t.kind())
        .chain(class.and_then(ItemClass::kind))
        .collect();
    if kinds.is_empty() {
        return Err(polymorphic_unresolved());
    }
    let target = common_element_type(&kinds).map_err(|e| match e {
        SqlSurfaceError::DatatypeMismatch { .. } => no_such_function(name),
        other => other,
    })?;
    let elem = CastType::Array(Box::new(target.clone()))
        .array_elem_type()
        .ok_or_else(|| unsupported("an array function on this element type is not supported"))?;
    Ok((target, elem))
}

/// 配列引数を共通の要素型の配列へ揃える（同じ要素型なら束縛結果をそのまま使う）。
fn coerce_array(
    arg: ArrArg<'_>,
    target: &CastType,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<BoundExpr, SqlSurfaceError> {
    match arg {
        ArrArg::Null => Ok(BoundExpr::Null),
        ArrArg::Array {
            expr,
            bound,
            ty,
            elem,
        } => {
            if &elem == target {
                return Ok(bound);
            }
            cast_bound(
                expr,
                bound,
                ty,
                &CastType::Array(Box::new(target.clone())),
                env,
                node_budget,
            )
            .map(|(b, _)| b)
        }
    }
}

/// 要素引数を共通の要素型へ変換する（分類で 1 度束縛したものは再束縛しない）。
fn coerce_elem(
    expr: &Expr,
    class: ItemClass,
    target: &CastType,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<BoundExpr, SqlSurfaceError> {
    match class {
        ItemClass::Bound { bound, ty, .. } => cast_bound(expr, bound, ty, target, env, node_budget),
        ItemClass::Null | ItemClass::Untyped | ItemClass::Kind(_) => {
            bind_cast(expr, target, env, node_budget)
        }
    }
    .map(|(b, _)| b)
}

/// 配列と要素を取る関数（append／prepend／position／remove）の共通束縛。返す束縛式は
/// （配列, 要素）の正準順、型は共通の要素型。
fn bind_array_and_elem(
    name: &str,
    arr_expr: &Expr,
    elem_expr: &Expr,
    needs_equality: bool,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, BoundExpr, ArrayElemType), SqlSurfaceError> {
    let arr = bind_array_arg(name, arr_expr, env, node_budget)?;
    let class = classify_item(elem_expr, env, node_budget)?;
    let arr_elems: Vec<&CastType> = arr.elem().into_iter().collect();
    let (target, elem) = resolve_target(name, &arr_elems, Some(&class))?;
    if needs_equality && target == CastType::Json {
        return Err(SqlSurfaceError::undefined_function(
            "could not identify an equality operator for type json",
        ));
    }
    let arr = coerce_array(arr, &target, env, node_budget)?;
    let el = coerce_elem(elem_expr, class, &target, env, node_budget)?;
    Ok((arr, el, elem))
}

/// 区切り文字・NULL 文字列の引数（text。型なし文字列リテラルと裸の `NULL` を受理）。
fn bind_text_arg(
    name: &str,
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<BoundExpr, SqlSurfaceError> {
    match expr {
        Expr::Null => {
            charge_node(node_budget)?;
            Ok(BoundExpr::Null)
        }
        Expr::String(lit) => {
            charge_node(node_budget)?;
            Ok(BoundExpr::Text(lit.clone()))
        }
        other => {
            let (bound, ty) = bind_expr_any(other, env, node_budget)?;
            if ty != ExprType::Text {
                return Err(no_such_function(name));
            }
            Ok(bound)
        }
    }
}

fn make(
    kind: ArrayFnKind,
    elem: Option<ArrayElemType>,
    args: Vec<BoundExpr>,
    ty: ExprType,
) -> (BoundExpr, ExprType) {
    (
        BoundExpr::ArrayFn(Box::new(BoundArrayFn { kind, elem, args })),
        ty,
    )
}

/// 配列関数 1 呼び出しの束縛（`name` は小文字化済み。`udf_call::bind_call` から呼ばれる）。
pub(super) fn bind_array_fn(
    name: &str,
    args: &[Expr],
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if crate::sql::udf_call::in_check_binding() {
        return Err(unsupported(
            "an array function is not supported in a CHECK constraint",
        ));
    }
    match name {
        "array_length" => {
            let [a, d] = args else {
                return Err(arity_error(name, "2", args.len()));
            };
            let arr = require_array(bind_array_arg(name, a, env, node_budget)?)?;
            let dim = bind_subscript_index(d, env, node_budget)?;
            Ok(make(
                ArrayFnKind::Length,
                None,
                vec![arr, dim],
                ExprType::Scalar,
            ))
        }
        "cardinality" => {
            let [a] = args else {
                return Err(arity_error(name, "1", args.len()));
            };
            let arr = require_array(bind_array_arg(name, a, env, node_budget)?)?;
            Ok(make(
                ArrayFnKind::Cardinality,
                None,
                vec![arr],
                ExprType::Scalar,
            ))
        }
        "array_append" | "array_prepend" => {
            let [x, y] = args else {
                return Err(arity_error(name, "2", args.len()));
            };
            let append = name == "array_append";
            let (arr_expr, elem_expr) = if append { (x, y) } else { (y, x) };
            let (arr, el, elem) =
                bind_array_and_elem(name, arr_expr, elem_expr, false, env, node_budget)?;
            let kind = if append {
                ArrayFnKind::Append
            } else {
                ArrayFnKind::Prepend
            };
            Ok(make(kind, Some(elem), vec![arr, el], ExprType::Array))
        }
        "array_cat" => {
            let [x, y] = args else {
                return Err(arity_error(name, "2", args.len()));
            };
            let a = bind_array_arg(name, x, env, node_budget)?;
            let b = bind_array_arg(name, y, env, node_budget)?;
            let elems: Vec<&CastType> = [a.elem(), b.elem()].into_iter().flatten().collect();
            let (target, elem) = resolve_target(name, &elems, None)?;
            let a = coerce_array(a, &target, env, node_budget)?;
            let b = coerce_array(b, &target, env, node_budget)?;
            Ok(make(
                ArrayFnKind::Cat,
                Some(elem),
                vec![a, b],
                ExprType::Array,
            ))
        }
        "array_position" => {
            let (a, e, s) = match args {
                [a, e] => (a, e, None),
                [a, e, s] => (a, e, Some(s)),
                _ => return Err(arity_error(name, "2 or 3", args.len())),
            };
            let (arr, el, elem) = bind_array_and_elem(name, a, e, true, env, node_budget)?;
            let mut bound = vec![arr, el];
            if let Some(s) = s {
                bound.push(bind_subscript_index(s, env, node_budget)?);
            }
            Ok(make(
                ArrayFnKind::Position,
                Some(elem),
                bound,
                ExprType::Scalar,
            ))
        }
        "array_remove" => {
            let [a, e] = args else {
                return Err(arity_error(name, "2", args.len()));
            };
            let (arr, el, elem) = bind_array_and_elem(name, a, e, true, env, node_budget)?;
            Ok(make(
                ArrayFnKind::Remove,
                Some(elem),
                vec![arr, el],
                ExprType::Array,
            ))
        }
        "array_to_string" => {
            let (a, d, n) = match args {
                [a, d] => (a, d, None),
                [a, d, n] => (a, d, Some(n)),
                _ => return Err(arity_error(name, "2 or 3", args.len())),
            };
            let arr = require_array(bind_array_arg(name, a, env, node_budget)?)?;
            let mut bound = vec![arr, bind_text_arg(name, d, env, node_budget)?];
            if let Some(n) = n {
                bound.push(bind_text_arg(name, n, env, node_budget)?);
            }
            Ok(make(ArrayFnKind::ToString, None, bound, ExprType::Text))
        }
        _ => Err(internal("unexpected array function name")),
    }
}

/// 結合結果の要素数・可変長本文の累計を確保前に検査して連結する。
fn join(first: &ArrayValue, second: &ArrayValue) -> Result<ArrayValue, SqlSurfaceError> {
    let total = first.len().checked_add(second.len());
    if total.is_none_or(|n| n > MAX_ARRAY_ELEMENTS as usize) {
        return Err(SqlSurfaceError::payload_too_large(
            "array function result has too many elements",
        ));
    }
    let payload = first
        .variable_payload_len()
        .saturating_add(second.variable_payload_len());
    if payload > MAX_SCALAR_PAYLOAD_LEN as usize {
        return Err(SqlSurfaceError::payload_too_large(
            "array function result payload is too large",
        ));
    }
    first
        .concat(second)
        .ok_or_else(|| internal("array function operands have different element types"))
}

/// 要素の評価値を 1 要素の配列へ（NULL は NULL 要素）。
fn one_element(v: ExprValue<'_>, elem: ArrayElemType) -> Result<ArrayValue, SqlSurfaceError> {
    values_to_array(vec![own(v)?], elem)
}

fn text_of(v: ExprValue<'_>) -> Result<Option<String>, SqlSurfaceError> {
    match v {
        ExprValue::Null => Ok(None),
        ExprValue::Text(s) => Ok(Some(s.into_owned())),
        _ => Err(internal("array function argument is not text")),
    }
}

/// 配列関数の評価。NULL・空配列の意味論は PostgreSQL に合わせる（`array_length`・`cardinality`・
/// `array_to_string` は strict、append／prepend／cat／position／remove は strict でない）。
pub(crate) fn eval_array_fn<'a>(
    f: &BoundArrayFn,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    let ev = |i: usize| -> Result<ExprValue<'a>, SqlSurfaceError> {
        let e = f
            .args
            .get(i)
            .ok_or_else(|| internal("array function argument is missing"))?;
        eval_with_scalars(e, id, embedding, row_scalars)
    };
    let need_elem = || {
        f.elem
            .ok_or_else(|| internal("array function element type is missing"))
    };
    match f.kind {
        ArrayFnKind::Length => {
            let a = array_of(ev(0)?)?;
            let d = index_value(&ev(1)?)?;
            let (Some(a), Some(d)) = (a, d) else {
                return Ok(ExprValue::Null);
            };
            // 配列は 1 次元のみ。空配列と 1 以外の次元は NULL。
            if d != 1 || a.len() == 0 {
                return Ok(ExprValue::Null);
            }
            Ok(ExprValue::Scalar(a.len() as f64))
        }
        ArrayFnKind::Cardinality => match array_of(ev(0)?)? {
            Some(a) => Ok(ExprValue::Scalar(a.len() as f64)),
            None => Ok(ExprValue::Null),
        },
        ArrayFnKind::Append | ArrayFnKind::Prepend => {
            let elem = need_elem()?;
            let base = match array_of(ev(0)?)? {
                Some(a) => a,
                None => values_to_array(Vec::new(), elem)?,
            };
            let one = one_element(ev(1)?, elem)?;
            let joined = if f.kind == ArrayFnKind::Append {
                join(&base, &one)?
            } else {
                join(&one, &base)?
            };
            Ok(ExprValue::Array(joined))
        }
        ArrayFnKind::Cat => match (array_of(ev(0)?)?, array_of(ev(1)?)?) {
            (None, None) => Ok(ExprValue::Null),
            (Some(a), None) | (None, Some(a)) => Ok(ExprValue::Array(a)),
            (Some(a), Some(b)) => Ok(ExprValue::Array(join(&a, &b)?)),
        },
        ArrayFnKind::Position => {
            let Some(a) = array_of(ev(0)?)? else {
                return Ok(ExprValue::Null);
            };
            let needle = one_element(ev(1)?, need_elem()?)?;
            let start = if f.args.len() > 2 {
                index_value(&ev(2)?)?.ok_or(SqlSurfaceError::NullValueNotAllowed {
                    detail: "initial position must not be null",
                })?
            } else {
                1
            };
            // 開始位置が 1 未満なら先頭から。長さを超えれば見つからない。
            let from = usize::try_from(start.max(1).saturating_sub(1)).unwrap_or(usize::MAX);
            let found = a
                .position_of(&needle, from)
                .map_err(|_| internal("array element comparison failed"))?;
            Ok(match found {
                Some(i) => ExprValue::Scalar(i.saturating_add(1) as f64),
                None => ExprValue::Null,
            })
        }
        ArrayFnKind::Remove => {
            let Some(a) = array_of(ev(0)?)? else {
                return Ok(ExprValue::Null);
            };
            let needle = one_element(ev(1)?, need_elem()?)?;
            let kept = a
                .remove_all(&needle)
                .map_err(|_| internal("array element comparison failed"))?;
            Ok(ExprValue::Array(kept))
        }
        ArrayFnKind::ToString => {
            let a = array_of(ev(0)?)?;
            let delim = text_of(ev(1)?)?;
            let (Some(a), Some(delim)) = (a, delim) else {
                return Ok(ExprValue::Null);
            };
            let null_text = match f.args.get(2) {
                Some(_) => text_of(ev(2)?)?,
                None => None,
            };
            let max = MAX_TEXT_FIELD_LEN as usize;
            let mut out = String::new();
            let mut first = true;
            for piece in a.element_texts() {
                let text = match (&piece, &null_text) {
                    (Some(s), _) => s.as_str(),
                    (None, Some(n)) => n.as_str(),
                    // NULL 文字列が無ければ NULL 要素は区切りも出さずに飛ばす。
                    (None, None) => continue,
                };
                let added = text
                    .len()
                    .saturating_add(if first { 0 } else { delim.len() });
                if out.len().saturating_add(added) > max {
                    return Err(SqlSurfaceError::payload_too_large(
                        "array_to_string result exceeds the maximum TEXT field length",
                    ));
                }
                if !first {
                    out.push_str(&delim);
                }
                first = false;
                out.push_str(text);
            }
            Ok(ExprValue::Text(Cow::Owned(out)))
        }
    }
}
