//! `jsonb_path_*` 関数（`jsonb_path_query_array`・`jsonb_path_query_first`・
//! `jsonb_path_exists`・`jsonb_path_match`）の束縛と評価（TABLE-25 ポインタ・Issue #2185）。
//!
//! 責務境界: `sql::udf_call` の `bind_call` が名前を [`is_jsonpath_fn_name`] で判定して
//! [`bind_jsonpath_fn`] へ委譲し、式評価器（`eval_with_scalars`）が束縛結果
//! （`BoundExpr::JsonPathFn`）を [`eval_jsonpath_fn`] で評価する。jsonpath の字句・構文解析
//! と評価の意味論は `crate::json::jsonpath`（Issue #2183・#2184）が担い、本モジュールは
//! 「SQL の引数規則・NULL 規則・エラー写像・結果の直列化上限」を足すだけの配線層である。
//!
//! 束縛時に確定するもの（評価時に再判定しない）:
//! - **path**: 型なし文字列リテラルだけを受理し、束縛時に `parse_jsonpath` する（PostgreSQL も
//!   unknown → jsonpath の変換は解析段で行うため、行が無い表でも構文エラーになる）。
//!   `jsonpath` 型そのもの（CAST・列）は後続の担当で、リテラル以外は `42883`。
//! - **silent**: 真偽リテラル・文字列・`NULL` の定数だけ（行ごとの評価は初版の対象外）。
//! - **上限**: [`JsonPathLimitsScope`] が文の束縛区間に張る `EngineCore::jsonpath_limits` を
//!   束縛時に読んで焼き込む。評価が並列ワーカーのスレッドで走っても束縛時の値を使う。
//!
//! 安全側の設計: 結果は `serialize_jsonb_bounded` で長さ・深さ・要素数を検査する（超過は
//! `54000`）。評価器のエラーは `SilentArg` で抑止できるものだけ評価器側が抑止し、`54000`・
//! `42704`・`22023`・`0A000` は常に呼び出し元へ返る。エラー文は評価器が持つ固定の英語文で、
//! 入力の断片を含めない。評価器は渡された値だけを読む純関数で、テナント情報に触れない。

use std::borrow::Cow;
use std::cell::Cell;
use std::sync::Arc;

use crate::json::jsonpath::{
    jsonpath_exists, jsonpath_match, jsonpath_query, parse_jsonpath, JsonPathEvalOptions,
    SqlJsonPath,
};
use crate::json::{serialize_jsonb_bounded, JsonValue};
use crate::jsonpath_limits::JsonPathLimits;
use crate::row_codec::ScalarRef;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::udf_call::{
    bind_json_jsonb_arg, json_build_error, BindEnv, BoundExpr, Expr, ExprType, ExprValue,
};

/// `jsonb_path_*` 関数の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonPathFnKind {
    QueryArray,
    QueryFirst,
    Exists,
    Match,
}

/// `silent` 引数の束縛結果（定数だけ。行ごとの評価は行わない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SilentArg {
    /// 省略（`false` 相当）。
    Omitted,
    /// 真偽の定数。
    Const(bool),
    /// 裸の `NULL`（strict なので結果は常に NULL）。
    Null,
}

/// `jsonb_path_*` 1 呼び出しの束縛結果（`BoundExpr::JsonPathFn` が保持する）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundJsonPathFn {
    pub(crate) kind: JsonPathFnKind,
    pub(crate) target: BoundExpr,
    /// `None` は path が裸の `NULL`（strict なので結果は常に NULL）。`Arc` は UDF 引数展開・
    /// ビューのインライン展開でのクローンを安くするため。
    pub(crate) path: Option<Arc<SqlJsonPath>>,
    pub(crate) vars: Option<BoundExpr>,
    pub(crate) silent: SilentArg,
    pub(crate) limits: JsonPathLimits,
}

/// 本モジュールが束縛する関数名（小文字）か。
pub(crate) fn is_jsonpath_fn_name(lower: &str) -> bool {
    matches!(
        lower,
        "jsonb_path_query_array"
            | "jsonb_path_query_first"
            | "jsonb_path_exists"
            | "jsonb_path_match"
    )
}

/// 述語として WHERE に裸で置ける関数名（大小無視。boolean を返す関数）か。
pub(crate) fn is_jsonpath_predicate_fn_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "jsonb_path_exists" | "jsonb_path_match"
    )
}

// ---------------------------------------------------------------------------
// 上限の運搬（文の束縛区間に限った thread-local の RAII スコープ）
// ---------------------------------------------------------------------------

thread_local! {
    /// 現在の束縛スコープの jsonpath 評価上限。未設定は `None`（ハード上限へフォールバック）。
    static CURRENT_LIMITS: Cell<Option<JsonPathLimits>> = const { Cell::new(None) };
}

/// `EngineCore::jsonpath_limits` を現スレッドの束縛スコープへ設定する RAII ガード。
/// `core.rs` の文実行入口（`time_fn::ClockScope` と同じ位置）が保持している間だけ有効で、
/// drop で直前の値へ戻す（入れ子・panic でも他文脈へ漏れない）。プロセス大域の状態ではない。
pub(crate) struct JsonPathLimitsScope {
    prev: Option<JsonPathLimits>,
}

impl JsonPathLimitsScope {
    pub(crate) fn enter(limits: JsonPathLimits) -> Self {
        let prev = CURRENT_LIMITS.with(|c| c.replace(Some(limits)));
        JsonPathLimitsScope { prev }
    }
}

impl Drop for JsonPathLimitsScope {
    fn drop(&mut self) {
        CURRENT_LIMITS.with(|c| c.set(self.prev));
    }
}

/// 現在の束縛スコープの上限。スコープが無い経路はハード上限（最大値。CLI は下げることしか
/// できないため、無制限にはならない）。
pub(crate) fn current_jsonpath_limits() -> JsonPathLimits {
    CURRENT_LIMITS.with(|c| c.get()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 束縛
// ---------------------------------------------------------------------------

fn charge_node(node_budget: &mut usize) -> Result<(), SqlSurfaceError> {
    *node_budget = node_budget
        .checked_sub(1)
        .ok_or_else(|| SqlSurfaceError::payload_too_large("expression is too large"))?;
    Ok(())
}

fn path_arg_error(name: &str) -> SqlSurfaceError {
    SqlSurfaceError::undefined_function(format!(
        "function {name} requires a jsonpath literal as its second argument"
    ))
}

/// path 引数の束縛。型なし文字列リテラルだけを束縛時に解析して受理する。
fn bind_path_arg(
    name: &str,
    expr: &Expr,
    node_budget: &mut usize,
) -> Result<Option<Arc<SqlJsonPath>>, SqlSurfaceError> {
    match expr {
        Expr::Null => {
            charge_node(node_budget)?;
            Ok(None)
        }
        Expr::String(s) => {
            charge_node(node_budget)?;
            let parsed = parse_jsonpath(s).map_err(SqlSurfaceError::JsonPathParse)?;
            Ok(Some(Arc::new(parsed)))
        }
        _ => Err(path_arg_error(name)),
    }
}

/// silent 引数の束縛（定数のみ）。boolean 列・一般式は `0A000`（fail-closed）。
fn bind_silent_arg(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<SilentArg, SqlSurfaceError> {
    let unsupported = || SqlSurfaceError::FeatureNotSupported {
        detail: "silent argument of jsonb_path functions must be a constant".to_string(),
    };
    match expr {
        Expr::Null => {
            charge_node(node_budget)?;
            Ok(SilentArg::Null)
        }
        Expr::String(s) => {
            charge_node(node_budget)?;
            crate::sql::params::parse_pg_bool(s)
                .map(SilentArg::Const)
                .ok_or_else(|| SqlSurfaceError::InvalidTextRepresentation {
                    detail: "invalid input syntax for type boolean".to_string(),
                })
        }
        Expr::Ident(w) => {
            // 真偽リテラルは字句段で Ident になる。列名・仮引数名と一致するものは boolean 列の
            // 参照かもしれないので、行ごとの評価が無い初版では拒否する。
            if env.has_identifier(w) {
                return Err(unsupported());
            }
            charge_node(node_budget)?;
            if w.eq_ignore_ascii_case("true") {
                return Ok(SilentArg::Const(true));
            }
            if w.eq_ignore_ascii_case("false") {
                return Ok(SilentArg::Const(false));
            }
            // 通常の列解決エラー（未定義列など）を返す。解決できてしまうなら未対応。
            bind_expr_for_error(expr, env, node_budget)?;
            Err(unsupported())
        }
        _ => {
            bind_expr_for_error(expr, env, node_budget)?;
            Err(unsupported())
        }
    }
}

/// 未対応形の引数でも、まず通常の束縛エラー（未定義列・型不一致等）を優先して返すための束縛。
fn bind_expr_for_error(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(), SqlSurfaceError> {
    crate::sql::udf_call::bind_expr_any(expr, env, node_budget).map(|_| ())
}

/// `jsonb_path_*` 1 呼び出しの束縛（`name` は小文字化済み）。
pub(super) fn bind_jsonpath_fn(
    name: &str,
    args: &[Expr],
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    if !(2..=4).contains(&args.len()) {
        return Err(SqlSurfaceError::undefined_function(format!(
            "function {name} expects 2 to 4 argument(s), got {}",
            args.len()
        )));
    }
    let (kind, ty) = match name {
        "jsonb_path_query_array" => (JsonPathFnKind::QueryArray, ExprType::Json),
        "jsonb_path_query_first" => (JsonPathFnKind::QueryFirst, ExprType::Json),
        "jsonb_path_exists" => (JsonPathFnKind::Exists, ExprType::Bool),
        "jsonb_path_match" => (JsonPathFnKind::Match, ExprType::Bool),
        _ => {
            return Err(SqlSurfaceError::Internal {
                detail: "unexpected jsonb_path function name".to_string(),
            })
        }
    };
    let mut it = args.iter();
    let (Some(target_arg), Some(path_arg)) = (it.next(), it.next()) else {
        return Err(SqlSurfaceError::Internal {
            detail: "jsonb_path argument is missing".to_string(),
        });
    };
    let target = bind_json_jsonb_arg(name, target_arg, env, node_budget)?;
    let path = bind_path_arg(name, path_arg, node_budget)?;
    let vars = match it.next() {
        Some(a) => Some(bind_json_jsonb_arg(name, a, env, node_budget)?),
        None => None,
    };
    let silent = match it.next() {
        Some(a) => bind_silent_arg(a, env, node_budget)?,
        None => SilentArg::Omitted,
    };
    Ok((
        BoundExpr::JsonPathFn(Box::new(BoundJsonPathFn {
            kind,
            target,
            path,
            vars,
            silent,
            limits: current_jsonpath_limits(),
        })),
        ty,
    ))
}

// ---------------------------------------------------------------------------
// 評価
// ---------------------------------------------------------------------------

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

/// jsonb 引数の評価値から正規化テキストを取り出す（NULL は `None`）。
fn jsonb_input<'a>(v: ExprValue<'a>) -> Result<Option<Cow<'a, str>>, SqlSurfaceError> {
    match v {
        ExprValue::Null => Ok(None),
        ExprValue::Json(t) => Ok(Some(t)),
        _ => Err(internal("jsonb_path argument is not a jsonb value")),
    }
}

/// `jsonb_path_*` の評価（`eval_with_scalars` から呼ばれる）。全関数が strict で、target・
/// path・vars・silent のどれかが SQL の NULL なら結果も NULL。
pub(crate) fn eval_jsonpath_fn<'a>(
    f: &BoundJsonPathFn,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    use crate::sql::udf_call::eval_with_scalars;
    let target = jsonb_input(eval_with_scalars(&f.target, id, embedding, row_scalars)?)?;
    let vars_text = match &f.vars {
        Some(v) => Some(jsonb_input(eval_with_scalars(
            v,
            id,
            embedding,
            row_scalars,
        )?)?),
        None => None,
    };
    let silent = match f.silent {
        SilentArg::Omitted => false,
        SilentArg::Const(b) => b,
        SilentArg::Null => return Ok(ExprValue::Null),
    };
    let (Some(target), Some(path)) = (target, f.path.as_ref()) else {
        return Ok(ExprValue::Null);
    };
    let vars_value: Option<JsonValue> = match vars_text {
        Some(Some(t)) => Some(crate::json::parse_jsonb_for_build(&t).map_err(json_build_error)?),
        Some(None) => return Ok(ExprValue::Null),
        None => None,
    };
    let root = crate::json::parse_jsonb_for_build(&target).map_err(json_build_error)?;
    let mut opts = JsonPathEvalOptions::new()
        .with_silent(silent)
        .with_limits(f.limits);
    if let Some(v) = vars_value.as_ref() {
        opts = opts.with_vars(v);
    }
    let eval_err = SqlSurfaceError::JsonPathEval;
    match f.kind {
        JsonPathFnKind::QueryArray => {
            let items = jsonpath_query(path, &root, opts).map_err(eval_err)?;
            let text =
                serialize_jsonb_bounded(&JsonValue::Array(items)).map_err(json_build_error)?;
            Ok(ExprValue::Json(Cow::Owned(text)))
        }
        JsonPathFnKind::QueryFirst => {
            // PostgreSQL も全件を評価してから先頭を返すため、後続項目のエラーも送出される。
            let items = jsonpath_query(path, &root, opts).map_err(eval_err)?;
            match items.first() {
                Some(v) => {
                    let text = serialize_jsonb_bounded(v).map_err(json_build_error)?;
                    Ok(ExprValue::Json(Cow::Owned(text)))
                }
                None => Ok(ExprValue::Null),
            }
        }
        JsonPathFnKind::Exists => Ok(jsonpath_exists(path, &root, opts)
            .map_err(eval_err)?
            .map_or(ExprValue::Null, ExprValue::Bool)),
        JsonPathFnKind::Match => Ok(jsonpath_match(path, &root, opts)
            .map_err(eval_err)?
            .map_or(ExprValue::Null, ExprValue::Bool)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_recognized() {
        assert!(is_jsonpath_fn_name("jsonb_path_exists"));
        assert!(is_jsonpath_fn_name("jsonb_path_query_first"));
        assert!(!is_jsonpath_fn_name("jsonb_path_query"));
        assert!(!is_jsonpath_fn_name("jsonb_path_exists_tz"));
        assert!(is_jsonpath_predicate_fn_name("JSONB_PATH_MATCH"));
        assert!(!is_jsonpath_predicate_fn_name("jsonb_path_query_array"));
    }

    #[test]
    fn limits_scope_nests_and_restores() {
        assert_eq!(current_jsonpath_limits(), JsonPathLimits::default());
        let small = JsonPathLimits::new(10, 4);
        {
            let _outer = JsonPathLimitsScope::enter(small);
            assert_eq!(current_jsonpath_limits(), small);
            {
                let tiny = JsonPathLimits::new(1, 1);
                let _inner = JsonPathLimitsScope::enter(tiny);
                assert_eq!(current_jsonpath_limits(), tiny);
            }
            assert_eq!(current_jsonpath_limits(), small);
        }
        assert_eq!(current_jsonpath_limits(), JsonPathLimits::default());
    }

    #[test]
    fn limits_scope_restores_after_panic() {
        let r = std::panic::catch_unwind(|| {
            let _s = JsonPathLimitsScope::enter(JsonPathLimits::new(3, 3));
            panic!("boom");
        });
        assert!(r.is_err());
        assert_eq!(current_jsonpath_limits(), JsonPathLimits::default());
    }

    #[test]
    fn serialized_results_are_canonical_fixed_points() {
        // 評価器が作る値（数値・文字列・入れ子）を含む結果が、jsonb の正規形として不変であること。
        let root = crate::json::parse_jsonb_for_build(r#"{"a":[1,2.50,"x"],"b":{"k":true}}"#)
            .expect("valid jsonb");
        for path in [
            "$.a[*]",
            "$.b.keyvalue()",
            "$.a[1] * 2",
            "$.**",
            "$.a.size()",
            "$.a[0] / 3",
        ] {
            let parsed = parse_jsonpath(path).expect("valid path");
            let items =
                jsonpath_query(&parsed, &root, JsonPathEvalOptions::new()).expect("evaluates");
            let text = serialize_jsonb_bounded(&JsonValue::Array(items)).expect("bounded");
            assert_eq!(
                crate::json::canonicalize_jsonb_text(&text).expect("re-canonicalizes"),
                text,
                "path {path}"
            );
        }
    }
}
