//! `json`（原文保存）型の関数群の束縛と評価（TABLE-25 ポインタ・Issue #2180）。
//!
//! 対象は `json_typeof`・`json_array_length`・`json_extract_path`・
//! `json_extract_path_text`・`json_build_object`・`json_build_array`・`to_json`。
//! `sql::udf_call` の `bind_call` が名前を [`is_json_fn_name`] で判定して
//! [`bind_json_fn`] へ委譲し、式評価器（`eval_with_scalars`）が束縛結果
//! （`BoundExpr::JsonFn`）を [`eval_json_fn`] で評価する。
//!
//! 固定アリティ・静的型の `BuiltinFn` には収まらない（可変長・任意型引数）ため別の
//! 束縛種別とした。json 値は `ExprType::JsonText`（原文の部分テキスト）で運び、
//! 部分テキストの切り出しは `crate::json_text` が担う。`json_build_*`／`to_json` の
//! 出力書式は PostgreSQL 17 の `json` に合わせる（オブジェクトは `{"a" : 1, "b" : 2}`、
//! 配列は `[1, 2]`）。埋め込む json 値は原文のまま、jsonb 値は正規化テキストのまま
//! 埋め込む（jsonb の PostgreSQL 出力書式との差は既知の対象外）。
//!
//! 安全側の設計: 出力は長さ（`MAX_JSON_FIELD_LEN`）・深さ・要素数の上限を検査し、超過は
//! `54000`。引数に NULL キーや不正な型があれば拒否する。エラー文に値・リテラル本文は含めない。

use std::borrow::Cow;

use crate::json::{validate_json_column_text, MAX_JSON_FIELD_LEN};
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::udf_call::{
    bind_expr_any, numeric_kind, BindEnv, BoundExpr, Expr, ExprType, ExprValue, NumericKind,
};

/// `json` 関数の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonFnKind {
    Typeof,
    ArrayLength,
    ExtractPath,
    ExtractPathText,
    BuildObject,
    BuildArray,
    ToJson,
}

/// 値を JSON テキストへ変換するときの引数の型（束縛時に確定。評価時に再判定しない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JsonArgKind {
    /// `json` 値（原文のまま埋め込む）。
    Json,
    /// `jsonb` 値（正規化テキストのまま埋め込む）。
    Jsonb,
    Text,
    /// 整数（int2／int4／int8）。
    Int,
    Float4,
    Float8,
    /// numeric リテラル（PostgreSQL は位取りを保つため字面から確定した出力）。
    Numeric(String),
    Bool,
    Date,
    Timestamp,
    /// 型なしの NULL リテラル。
    Null,
}

/// `json` 関数の束縛結果（`BoundExpr::JsonFn` が保持する）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundJsonFn {
    pub(crate) kind: JsonFnKind,
    pub(crate) args: Vec<BoundExpr>,
    pub(crate) arg_kinds: Vec<JsonArgKind>,
}

impl BoundJsonFn {
    /// 結果が int4 か（`json_array_length`。結果列の型 OID を 23 にするため）。
    pub(crate) fn result_is_int4(&self) -> bool {
        self.kind == JsonFnKind::ArrayLength
    }
}

/// 本モジュールが扱う関数名（小文字）か。集合返却関数（`json_object_keys` 等）も名前を
/// 予約し、束縛で `0A000` を返す。
pub(crate) fn is_json_fn_name(lower: &str) -> bool {
    matches!(
        lower,
        "json_typeof"
            | "json_array_length"
            | "json_extract_path"
            | "json_extract_path_text"
            | "json_build_object"
            | "json_build_array"
            | "to_json"
            | "json_object_keys"
            | "json_each"
            | "json_each_text"
            | "json_array_elements"
            | "json_array_elements_text"
    )
}

fn charge_node(node_budget: &mut usize) -> Result<(), SqlSurfaceError> {
    *node_budget = node_budget
        .checked_sub(1)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
    Ok(())
}

fn arity_error(name: &str, expected: &str, got: usize) -> SqlSurfaceError {
    SqlSurfaceError::undefined_function(format!(
        "function {name} expects {expected} argument(s), got {got}"
    ))
}

/// json を要求する引数位置の束縛。型なし文字列リテラルは json として検証して受理する
/// （PostgreSQL の unknown → json 解決。不正な JSON は `22P02`、長さ超過は `54000`）。
/// それ以外は `ExprType::JsonText` の式のみ（text・jsonb は `42883`）。
fn bind_json_input(
    name: &str,
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<BoundExpr, SqlSurfaceError> {
    // 裸の `NULL` は型なしの NULL（strict な関数なので結果も NULL）。
    if matches!(expr, Expr::Null) {
        charge_node(node_budget)?;
        return Ok(BoundExpr::Null);
    }
    if let Expr::String(lit) = expr {
        charge_node(node_budget)?;
        validate_json_column_text(lit)
            .map_err(|e| crate::sql::parser::json_column_error(e, lit))?;
        return Ok(BoundExpr::Text(lit.clone()));
    }
    let (bound, ty) = bind_expr_any(expr, env, node_budget)?;
    if ty != ExprType::JsonText {
        return Err(SqlSurfaceError::undefined_function(format!(
            "function {name} does not exist for the given argument types"
        )));
    }
    Ok(bound)
}

/// 値位置（`json_build_*`・`to_json` の引数）の束縛と、JSON テキスト化用の型確定。
fn bind_value_arg(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, JsonArgKind), SqlSurfaceError> {
    if matches!(expr, Expr::Null) {
        charge_node(node_budget)?;
        return Ok((BoundExpr::Null, JsonArgKind::Null));
    }
    let (bound, ty) = bind_expr_any(expr, env, node_budget)?;
    let kind = match ty {
        ExprType::Scalar => match numeric_kind(expr, &bound, env) {
            Some(NumericKind::Int2 | NumericKind::Int4 | NumericKind::Int8) => JsonArgKind::Int,
            Some(NumericKind::Float4) => JsonArgKind::Float4,
            None => match expr {
                // numeric リテラルは位取りを保って出力する（`1.50` は `1.50`）。
                Expr::Number(raw) => match crate::numeric::parse_literal_exact(raw) {
                    Ok(d) => JsonArgKind::Numeric(d.to_string()),
                    Err(_) => JsonArgKind::Float8,
                },
                _ => JsonArgKind::Float8,
            },
        },
        ExprType::Bool => JsonArgKind::Bool,
        ExprType::Text => JsonArgKind::Text,
        ExprType::Date => JsonArgKind::Date,
        ExprType::Timestamp => JsonArgKind::Timestamp,
        ExprType::Json => JsonArgKind::Jsonb,
        ExprType::JsonText => JsonArgKind::Json,
        ExprType::Vector => {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: "a vector value cannot be converted to json".to_string(),
            })
        }
        // TIME の json 化は未対応（#1570 の対象外）。fail-closed で拒否する。
        ExprType::Time => {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: "a time value cannot be converted to json yet".to_string(),
            })
        }
    };
    Ok((bound, kind))
}

/// `json` 関数 1 呼び出しの束縛（`name` は小文字化済み）。
pub(super) fn bind_json_fn(
    name: &str,
    args: &[Expr],
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    match name {
        "json_object_keys"
        | "json_each"
        | "json_each_text"
        | "json_array_elements"
        | "json_array_elements_text" => Err(SqlSurfaceError::FeatureNotSupported {
            detail: "set-returning JSON functions are not supported".to_string(),
        }),
        "json_typeof" | "json_array_length" => {
            let [arg] = args else {
                return Err(arity_error(name, "1", args.len()));
            };
            let bound = bind_json_input(name, arg, env, node_budget)?;
            let (kind, ty) = if name == "json_typeof" {
                (JsonFnKind::Typeof, ExprType::Text)
            } else {
                (JsonFnKind::ArrayLength, ExprType::Scalar)
            };
            Ok((make(kind, vec![bound], vec![JsonArgKind::Json]), ty))
        }
        "json_extract_path" | "json_extract_path_text" => {
            // VARIADIC text。パス要素が 0 個の呼び出しは PostgreSQL でも未定義（`42883`）。
            if args.len() < 2 {
                return Err(arity_error(name, "at least 2", args.len()));
            }
            let mut bound = Vec::with_capacity(args.len());
            let mut kinds = Vec::with_capacity(args.len());
            for (i, a) in args.iter().enumerate() {
                if i == 0 {
                    bound.push(bind_json_input(name, a, env, node_budget)?);
                    kinds.push(JsonArgKind::Json);
                } else {
                    // `NULL` 要素は結果 NULL（strict）。型なし NULL も text として受理する。
                    if matches!(a, Expr::Null) {
                        charge_node(node_budget)?;
                        bound.push(BoundExpr::Null);
                    } else {
                        let (b, ty) = bind_expr_any(a, env, node_budget)?;
                        if ty != ExprType::Text {
                            return Err(SqlSurfaceError::undefined_function(format!(
                                "function {name} path elements must be text"
                            )));
                        }
                        bound.push(b);
                    }
                    kinds.push(JsonArgKind::Text);
                }
            }
            let (kind, ty) = if name == "json_extract_path" {
                (JsonFnKind::ExtractPath, ExprType::JsonText)
            } else {
                (JsonFnKind::ExtractPathText, ExprType::Text)
            };
            Ok((make(kind, bound, kinds), ty))
        }
        "json_build_object" | "json_build_array" | "to_json" => {
            if name == "to_json" && args.len() != 1 {
                return Err(arity_error(name, "1", args.len()));
            }
            if name == "json_build_object" && !args.len().is_multiple_of(2) {
                return Err(SqlSurfaceError::InvalidParameterValue {
                    detail: "argument list must have even number of elements",
                });
            }
            let mut bound = Vec::with_capacity(args.len());
            let mut kinds = Vec::with_capacity(args.len());
            for (i, a) in args.iter().enumerate() {
                let (b, k) = bind_value_arg(a, env, node_budget)?;
                // オブジェクトのキーは json・jsonb 値を取れない（`22023`）。
                if name == "json_build_object"
                    && i % 2 == 0
                    && matches!(k, JsonArgKind::Json | JsonArgKind::Jsonb)
                {
                    return Err(SqlSurfaceError::InvalidParameterValue {
                        detail: "key value must be scalar, not array, composite, or json",
                    });
                }
                bound.push(b);
                kinds.push(k);
            }
            let kind = match name {
                "json_build_object" => JsonFnKind::BuildObject,
                "json_build_array" => JsonFnKind::BuildArray,
                _ => JsonFnKind::ToJson,
            };
            Ok((make(kind, bound, kinds), ExprType::JsonText))
        }
        _ => Err(SqlSurfaceError::Internal {
            detail: "unexpected json function name".to_string(),
        }),
    }
}

fn make(kind: JsonFnKind, args: Vec<BoundExpr>, arg_kinds: Vec<JsonArgKind>) -> BoundExpr {
    BoundExpr::JsonTextFn(Box::new(BoundJsonFn {
        kind,
        args,
        arg_kinds,
    }))
}

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

fn too_large() -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large("json value exceeds the allowed size")
}

/// json 入力引数の評価値からテキストを取り出す（NULL は `None`）。
fn json_input<'a>(v: ExprValue<'a>) -> Result<Option<Cow<'a, str>>, SqlSurfaceError> {
    match v {
        ExprValue::Null => Ok(None),
        ExprValue::JsonText(t) | ExprValue::Text(t) => Ok(Some(t)),
        _ => Err(internal("json function argument is not a json value")),
    }
}

fn scan_error(_: crate::json::JsonError) -> SqlSurfaceError {
    internal("stored json value could not be parsed")
}

/// `out` へ追記する（長さ上限を超える場合は確保前に `54000`）。
fn push_checked(out: &mut String, piece: &str) -> Result<(), SqlSurfaceError> {
    let new_len = out.len().checked_add(piece.len()).ok_or_else(too_large)?;
    if new_len > MAX_JSON_FIELD_LEN {
        return Err(too_large());
    }
    out.try_reserve(piece.len()).map_err(|_| too_large())?;
    out.push_str(piece);
    Ok(())
}

/// 非有限の浮動小数は PostgreSQL に合わせて JSON 文字列（`"NaN"` 等）にする。
fn float_json(text: String, finite: bool) -> String {
    if finite {
        text
    } else {
        format!("\"{text}\"")
    }
}

/// 値のテキスト表現（オブジェクトのキー用。JSON 引用符は付けない）。
fn value_plain_text(kind: &JsonArgKind, v: &ExprValue<'_>) -> Result<String, SqlSurfaceError> {
    match (kind, v) {
        (JsonArgKind::Text, ExprValue::Text(t)) => Ok(t.to_string()),
        (JsonArgKind::Int, ExprValue::Scalar(x)) => int_text(*x),
        (JsonArgKind::Float4, ExprValue::Scalar(x)) => {
            Ok(crate::scalar_float::format_real(*x as f32))
        }
        (JsonArgKind::Float8, ExprValue::Scalar(x)) => Ok(crate::scalar_float::format_double(*x)),
        (JsonArgKind::Numeric(n), ExprValue::Scalar(_)) => Ok(n.clone()),
        (JsonArgKind::Bool, ExprValue::Bool(b)) => Ok(b.to_string()),
        (JsonArgKind::Date, ExprValue::Date(d)) => Ok(crate::datetime::format_date(*d)),
        (JsonArgKind::Timestamp, ExprValue::Timestamp(t)) => {
            // 値位置と同じ ISO 8601（`T` 区切り）に揃える。
            Ok(crate::json::json_timestamp_text(*t))
        }
        _ => Err(internal("json function argument type mismatch")),
    }
}

fn int_text(x: f64) -> Result<String, SqlSurfaceError> {
    // 2^63 は f64 で厳密に表現できる。範囲外・非整数は内部不整合として拒否する。
    if !x.is_finite() || x.fract() != 0.0 || x.abs() >= 9_223_372_036_854_775_808.0 {
        return Err(SqlSurfaceError::numeric_out_of_range(
            "integer value is out of range for json conversion",
        ));
    }
    Ok((x as i64).to_string())
}

/// 値 1 個を JSON テキストへ書く（NULL は `null`）。
fn value_to_json(
    kind: &JsonArgKind,
    v: &ExprValue<'_>,
    out: &mut String,
) -> Result<(), SqlSurfaceError> {
    if matches!(v, ExprValue::Null) {
        return push_checked(out, "null");
    }
    match (kind, v) {
        (JsonArgKind::Json, ExprValue::JsonText(t)) => push_checked(out, t),
        (JsonArgKind::Jsonb, ExprValue::Json(t)) => push_checked(out, t),
        (JsonArgKind::Text, ExprValue::Text(t)) => {
            // 引用・エスケープ後の長さは入力の高々 6 倍。確保前に上限を見る。
            if t.len() > MAX_JSON_FIELD_LEN {
                return Err(too_large());
            }
            let mut quoted = String::new();
            quoted.try_reserve(t.len() + 2).map_err(|_| too_large())?;
            crate::json::write_canonical_string(t, &mut quoted);
            push_checked(out, &quoted)
        }
        (JsonArgKind::Int, ExprValue::Scalar(x)) => push_checked(out, &int_text(*x)?),
        (JsonArgKind::Float4, ExprValue::Scalar(x)) => {
            let f = *x as f32;
            push_checked(
                out,
                &float_json(crate::scalar_float::format_real(f), f.is_finite()),
            )
        }
        (JsonArgKind::Float8, ExprValue::Scalar(x)) => push_checked(
            out,
            &float_json(crate::scalar_float::format_double(*x), x.is_finite()),
        ),
        (JsonArgKind::Numeric(n), ExprValue::Scalar(_)) => push_checked(out, n),
        (JsonArgKind::Bool, ExprValue::Bool(b)) => {
            push_checked(out, if *b { "true" } else { "false" })
        }
        (JsonArgKind::Date, ExprValue::Date(d)) => {
            push_checked(out, &format!("\"{}\"", crate::datetime::format_date(*d)))
        }
        (JsonArgKind::Timestamp, ExprValue::Timestamp(t)) => {
            // PostgreSQL の json は ISO 8601（日付と時刻の区切りは `T`）。
            let text = crate::json::json_timestamp_text(*t);
            push_checked(out, &format!("\"{text}\""))
        }
        _ => Err(internal("json function argument type mismatch")),
    }
}

/// 組み立てた出力を検証する。json／jsonb の埋め込みの有無に関わらず、格納時と同じ
/// 深さ・要素数・文字列長の上限に収まらなければ `54000`（そのまま格納・再パースできない
/// 値を作らない。text 由来の長大な文字列も対象）。
fn finish(out: String) -> Result<ExprValue<'static>, SqlSurfaceError> {
    if validate_json_column_text(&out).is_err() {
        return Err(SqlSurfaceError::payload_too_large(
            "json value exceeds the allowed nesting depth or size",
        ));
    }
    Ok(ExprValue::JsonText(Cow::Owned(out)))
}

/// `json` 関数の評価（`eval_with_scalars` から呼ばれる）。strict な関数（typeof・
/// array_length・extract_path*・to_json）は NULL 入力で NULL。`json_build_*` は NULL を
/// `null` として書く（キーの NULL は `22004`）。
pub(crate) fn eval_json_fn<'a>(
    f: &BoundJsonFn,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    use crate::sql::udf_call::eval_with_scalars;
    match f.kind {
        JsonFnKind::Typeof | JsonFnKind::ArrayLength => {
            let Some(arg) = f.args.first() else {
                return Err(internal("json function argument is missing"));
            };
            let Some(text) = json_input(eval_with_scalars(arg, id, embedding, row_scalars)?)?
            else {
                return Ok(ExprValue::Null);
            };
            if f.kind == JsonFnKind::Typeof {
                let label = crate::json_text::json_text_typeof(&text).map_err(scan_error)?;
                return Ok(ExprValue::Text(Cow::Owned(label.to_string())));
            }
            match crate::json_text::json_text_array_length(&text).map_err(scan_error)? {
                Some(n) => Ok(ExprValue::Scalar(n as f64)),
                None => Err(SqlSurfaceError::InvalidParameterValue {
                    detail: "cannot get array length of a non-array",
                }),
            }
        }
        JsonFnKind::ExtractPath | JsonFnKind::ExtractPathText => {
            let Some((first, rest)) = f.args.split_first() else {
                return Err(internal("json function argument is missing"));
            };
            let Some(text) = json_input(eval_with_scalars(first, id, embedding, row_scalars)?)?
            else {
                return Ok(ExprValue::Null);
            };
            let mut path: Vec<Option<String>> = Vec::new();
            path.try_reserve(rest.len()).map_err(|_| too_large())?;
            for a in rest {
                match eval_with_scalars(a, id, embedding, row_scalars)? {
                    ExprValue::Text(t) => path.push(Some(t.into_owned())),
                    ExprValue::Null => path.push(None),
                    _ => return Err(internal("json path element is not a text value")),
                }
            }
            let Some(span) =
                crate::json_text::json_text_get_path(&text, &path).map_err(scan_error)?
            else {
                return Ok(ExprValue::Null);
            };
            if f.kind == JsonFnKind::ExtractPath {
                return Ok(ExprValue::JsonText(Cow::Owned(span.to_string())));
            }
            Ok(
                match crate::json_text::json_text_as_text(span).map_err(scan_error)? {
                    Some(t) => ExprValue::Text(Cow::Owned(t)),
                    None => ExprValue::Null,
                },
            )
        }
        JsonFnKind::ToJson => {
            let (Some(arg), Some(kind)) = (f.args.first(), f.arg_kinds.first()) else {
                return Err(internal("json function argument is missing"));
            };
            let v = eval_with_scalars(arg, id, embedding, row_scalars)?;
            if matches!(v, ExprValue::Null) {
                return Ok(ExprValue::Null);
            }
            let mut out = String::new();
            value_to_json(kind, &v, &mut out)?;
            finish(out)
        }
        JsonFnKind::BuildArray => {
            let mut out = String::new();
            push_checked(&mut out, "[")?;
            for (i, (a, kind)) in f.args.iter().zip(f.arg_kinds.iter()).enumerate() {
                if i > 0 {
                    push_checked(&mut out, ", ")?;
                }
                let v = eval_with_scalars(a, id, embedding, row_scalars)?;
                value_to_json(kind, &v, &mut out)?;
            }
            push_checked(&mut out, "]")?;
            finish(out)
        }
        JsonFnKind::BuildObject => {
            let mut out = String::new();
            push_checked(&mut out, "{")?;
            let mut pairs = f.args.iter().zip(f.arg_kinds.iter());
            let mut first = true;
            while let (Some((ka, kk)), Some((va, vk))) = (pairs.next(), pairs.next()) {
                if !first {
                    push_checked(&mut out, ", ")?;
                }
                first = false;
                let key = eval_with_scalars(ka, id, embedding, row_scalars)?;
                if matches!(key, ExprValue::Null) {
                    return Err(SqlSurfaceError::NullValueNotAllowed {
                        detail: "null value not allowed for object key",
                    });
                }
                let key_text = value_plain_text(kk, &key)?;
                let mut quoted = String::new();
                if key_text.len() > MAX_JSON_FIELD_LEN {
                    return Err(too_large());
                }
                crate::json::write_canonical_string(&key_text, &mut quoted);
                push_checked(&mut out, &quoted)?;
                push_checked(&mut out, " : ")?;
                let v = eval_with_scalars(va, id, embedding, row_scalars)?;
                value_to_json(vk, &v, &mut out)?;
            }
            push_checked(&mut out, "}")?;
            finish(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_fn_names_are_recognized_case_normalized() {
        assert!(is_json_fn_name("to_json"));
        assert!(is_json_fn_name("json_object_keys"));
        assert!(!is_json_fn_name("jsonb_typeof"));
    }

    #[test]
    fn push_checked_rejects_output_beyond_the_json_field_limit() {
        let mut out = String::new();
        let big = "x".repeat(MAX_JSON_FIELD_LEN);
        assert!(push_checked(&mut out, &big).is_ok());
        assert!(push_checked(&mut out, "y").is_err());
    }

    #[test]
    fn scalar_values_render_like_postgres_json() {
        let mut out = String::new();
        value_to_json(
            &JsonArgKind::Int,
            &ExprValue::Scalar(12345678901.0),
            &mut out,
        )
        .unwrap();
        assert_eq!(out, "12345678901");
        let mut out = String::new();
        value_to_json(&JsonArgKind::Float8, &ExprValue::Scalar(1e20), &mut out).unwrap();
        assert_eq!(out, "1e+20");
        let mut out = String::new();
        value_to_json(&JsonArgKind::Float8, &ExprValue::Scalar(f64::NAN), &mut out).unwrap();
        assert_eq!(out, "\"NaN\"");
        let mut out = String::new();
        value_to_json(
            &JsonArgKind::Numeric("1.50".to_string()),
            &ExprValue::Scalar(1.5),
            &mut out,
        )
        .unwrap();
        assert_eq!(out, "1.50");
        let mut out = String::new();
        value_to_json(&JsonArgKind::Timestamp, &ExprValue::Timestamp(0), &mut out).unwrap();
        assert_eq!(out, "\"1970-01-01T00:00:00\"");
        let mut out = String::new();
        value_to_json(
            &JsonArgKind::Text,
            &ExprValue::Text(Cow::Borrowed("x\"y")),
            &mut out,
        )
        .unwrap();
        assert_eq!(out, "\"x\\\"y\"");
    }

    #[test]
    fn timestamp_object_key_uses_iso8601_t_separator() {
        let key = value_plain_text(&JsonArgKind::Timestamp, &ExprValue::Timestamp(0)).unwrap();
        assert_eq!(key, "1970-01-01T00:00:00");
    }

    #[test]
    fn int_text_rejects_non_integers_and_out_of_range() {
        assert!(int_text(1.5).is_err());
        assert!(int_text(f64::INFINITY).is_err());
        assert!(int_text(9.3e18).is_err());
        assert_eq!(int_text(-7.0).unwrap(), "-7");
    }

    #[test]
    fn finish_rejects_overlong_string_without_embedded_json() {
        // text 由来の文字列が MAX_JSON_STRING_CHARS を超えるとき、生成段階で 54000 になる。
        let out = format!(
            "[\"{}\"]",
            "a".repeat(crate::json::MAX_JSON_STRING_CHARS + 1)
        );
        assert!(out.len() <= MAX_JSON_FIELD_LEN);
        assert!(finish(out).is_err());
        assert!(finish("[\"ok\"]".to_string()).is_ok());
    }
}
