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
    SqlJsonBehavior, SqlJsonQueryExpr, SqlJsonQueryKind,
};

/// `jsonb_path_*` 関数の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JsonPathFnKind {
    QueryArray,
    QueryFirst,
    Exists,
    Match,
    /// `JSON_EXISTS`（句は [`BoundJsonPathFn::sqljson`]。Issue #2187）。
    SqlExists,
    /// `JSON_VALUE`（Issue #2187）。
    SqlValue,
    /// `JSON_QUERY`（Issue #2187）。
    SqlQuery,
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
    /// SQL/JSON 問い合わせ関数（`JSON_EXISTS`・`JSON_VALUE`・`JSON_QUERY`。Issue #2187）の句。
    /// `jsonb_path_*` では `None`。
    pub(crate) sqljson: Option<Box<BoundSqlJsonClauses>>,
}

impl BoundJsonPathFn {
    /// `PASSING` の値式（`jsonb_path_*` では空）。式木の走査系が部分式を取りこぼさないよう共有する。
    pub(crate) fn passing_exprs(&self) -> impl Iterator<Item = &BoundExpr> {
        self.sqljson
            .iter()
            .flat_map(|c| c.passing.iter().map(|(_, e)| e))
    }
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
            sqljson: None,
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
    if let Some(c) = &f.sqljson {
        return eval_sql_json(f, c, id, embedding, row_scalars);
    }
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
        // 句つきの種別は先頭で `eval_sql_json` へ振り分け済み（到達しない）。
        JsonPathFnKind::SqlExists | JsonPathFnKind::SqlValue | JsonPathFnKind::SqlQuery => Err(
            internal("SQL/JSON query function reached the jsonb_path evaluator"),
        ),
    }
}
// ---------------------------------------------------------------------------
// SQL/JSON 問い合わせ関数（JSON_EXISTS・JSON_VALUE・JSON_QUERY。TABLE-25・TASK-273・Issue #2187）
// ---------------------------------------------------------------------------
//
// 構文段（`allowlist::Parser::parse_sql_json_query`）が [`Expr::SqlJsonQuery`] として句を構造のまま
// 渡し、[`bind_sql_json_query`] が受理範囲を決めて [`BoundJsonPathFn`] へ束縛する。評価は同じ
// jsonpath 評価器（`crate::json::jsonpath`）を**非 silent** で呼び、抑止できる評価エラー
// （`JsonPathEvalError::is_suppressible`）だけを `ON ERROR` の対象にする。抑止できないエラー
// （上限超過 `54000`・未定義変数 `42704`・未対応構文 `0A000`・内部不整合）は句の指定に関係なく
// 呼び出し元へ返る（fail-closed）。意味を確かめていない句・型はすべて束縛時に `0A000` で拒否する。

/// `RETURNING` で受理した結果型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SqlJsonRet {
    /// `text`／長さなしの `varchar`。
    Text,
    /// `jsonb`（`JSON_QUERY` の既定）。
    Jsonb,
    /// `json`（jsonb の正準テキストを json として返す）。
    Json,
}

/// 束縛時に結果型へ変換済みの `DEFAULT` 定数。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BoundDefault {
    Null,
    Text(String),
    /// jsonb の正準テキスト。
    Json(String),
}

/// 束縛済みの `ON EMPTY`／`ON ERROR` の挙動。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BoundBehavior {
    Error,
    Null,
    True,
    False,
    Unknown,
    EmptyArray,
    EmptyObject,
    Default(BoundDefault),
}

/// SQL/JSON 問い合わせ関数の句の束縛結果（[`BoundJsonPathFn::sqljson`]）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSqlJsonClauses {
    pub(crate) ret: SqlJsonRet,
    pub(crate) with_wrapper: bool,
    pub(crate) omit_quotes: bool,
    pub(crate) on_empty: BoundBehavior,
    pub(crate) on_error: BoundBehavior,
    /// `PASSING` の（変数名, 値式）。名前は束縛時に重複なしを確認済み。
    pub(crate) passing: Vec<(String, BoundExpr)>,
}

fn sqljson_unsupported(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: detail.to_string(),
    }
}

/// `RETURNING` の型名を関数ごとの受理表で判定する（PostgreSQL の入力関数による変換との一致を
/// まだ保証できない型はすべて `0A000`）。
fn bind_sql_json_returning(
    kind: SqlJsonQueryKind,
    returning: Option<&str>,
) -> Result<SqlJsonRet, SqlSurfaceError> {
    match (kind, returning) {
        (SqlJsonQueryKind::Exists, Some(_)) => Err(sqljson_unsupported(
            "RETURNING is not supported in JSON_EXISTS",
        )),
        (SqlJsonQueryKind::Exists, None) => Ok(SqlJsonRet::Text),
        (SqlJsonQueryKind::Value, None | Some("text" | "varchar")) => Ok(SqlJsonRet::Text),
        (SqlJsonQueryKind::Query, None | Some("jsonb")) => Ok(SqlJsonRet::Jsonb),
        (SqlJsonQueryKind::Query, Some("json")) => Ok(SqlJsonRet::Json),
        (SqlJsonQueryKind::Query, Some("text" | "varchar")) => Ok(SqlJsonRet::Text),
        _ => Err(sqljson_unsupported(
            "this RETURNING type is not supported by SQL/JSON query functions",
        )),
    }
}

/// `DEFAULT` 式を結果型へ束縛時に変換して定数にする（列参照・一般式は `0A000`）。
fn bind_sql_json_default(
    kind: SqlJsonQueryKind,
    ret: SqlJsonRet,
    expr: &Expr,
    node_budget: &mut usize,
) -> Result<BoundDefault, SqlSurfaceError> {
    charge_node(node_budget)?;
    match expr {
        Expr::Null => Ok(BoundDefault::Null),
        Expr::String(s) => match ret {
            SqlJsonRet::Text => Ok(BoundDefault::Text(s.clone())),
            SqlJsonRet::Jsonb | SqlJsonRet::Json => crate::json::canonicalize_jsonb_text(s)
                .map(BoundDefault::Json)
                .map_err(|e| match e {
                    crate::json::JsonColumnError::TooLong => {
                        SqlSurfaceError::payload_too_large("jsonb result exceeds the allowed size")
                    }
                    crate::json::JsonColumnError::Invalid => {
                        SqlSurfaceError::InvalidTextRepresentation {
                            detail: "invalid input syntax for type json".to_string(),
                        }
                    }
                }),
        },
        // text 系の結果型に限り、先頭ゼロなしの整数リテラルだけを数値のテキスト表記のまま受理する
        // （PostgreSQL の numeric→text 出力と一致する範囲）。
        Expr::Number(raw)
            if kind == SqlJsonQueryKind::Value
                && ret == SqlJsonRet::Text
                && !raw.is_empty()
                && raw.bytes().all(|b| b.is_ascii_digit())
                && (raw == "0" || !raw.starts_with('0')) =>
        {
            Ok(BoundDefault::Text(raw.clone()))
        }
        _ => Err(sqljson_unsupported(
            "DEFAULT of SQL/JSON query functions must be a string literal or NULL",
        )),
    }
}

/// 関数ごとに許される挙動だけを受理して束縛する（合わない挙動は PostgreSQL と同じ構文エラー）。
fn bind_sql_json_behavior(
    kind: SqlJsonQueryKind,
    ret: SqlJsonRet,
    b: Option<&SqlJsonBehavior>,
    node_budget: &mut usize,
) -> Result<BoundBehavior, SqlSurfaceError> {
    let Some(b) = b else {
        return Ok(match kind {
            SqlJsonQueryKind::Exists => BoundBehavior::False,
            _ => BoundBehavior::Null,
        });
    };
    let invalid =
        || SqlSurfaceError::unsupported("invalid ON EMPTY/ON ERROR behavior for function");
    let allowed = match (kind, b) {
        (SqlJsonQueryKind::Exists, SqlJsonBehavior::Error | SqlJsonBehavior::True)
        | (SqlJsonQueryKind::Exists, SqlJsonBehavior::False | SqlJsonBehavior::Unknown) => true,
        (SqlJsonQueryKind::Exists, _) => false,
        (_, SqlJsonBehavior::Error | SqlJsonBehavior::Null | SqlJsonBehavior::Default(_)) => true,
        (SqlJsonQueryKind::Query, SqlJsonBehavior::EmptyArray | SqlJsonBehavior::EmptyObject) => {
            true
        }
        _ => false,
    };
    if !allowed {
        return Err(invalid());
    }
    Ok(match b {
        SqlJsonBehavior::Error => BoundBehavior::Error,
        SqlJsonBehavior::Null => BoundBehavior::Null,
        SqlJsonBehavior::True => BoundBehavior::True,
        SqlJsonBehavior::False => BoundBehavior::False,
        SqlJsonBehavior::Unknown => BoundBehavior::Unknown,
        SqlJsonBehavior::EmptyArray => BoundBehavior::EmptyArray,
        SqlJsonBehavior::EmptyObject => BoundBehavior::EmptyObject,
        SqlJsonBehavior::Default(e) => {
            BoundBehavior::Default(bind_sql_json_default(kind, ret, e, node_budget)?)
        }
    })
}

/// context item の束縛。jsonb の式・型なし文字列リテラル・裸の `NULL` だけを受理し、json（原文保存）
/// と text 系ほかは `0A000`（json→jsonb 変換と不正 JSON の写像が未整備のため）。
fn bind_sql_json_context(
    name: &str,
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<BoundExpr, SqlSurfaceError> {
    match expr {
        Expr::Null | Expr::String(_) => bind_json_jsonb_arg(name, expr, env, node_budget),
        _ => {
            let (bound, ty) = crate::sql::udf_call::bind_expr_any(expr, env, node_budget)?;
            if ty != ExprType::Json {
                return Err(sqljson_unsupported(
                    "the context item of SQL/JSON query functions must be a jsonb value",
                ));
            }
            Ok(bound)
        }
    }
}

/// `PASSING` 値の束縛。jsonb・text・数値・boolean・`NULL`（JSON の `null`）だけを受理する。
fn bind_sql_json_passing(
    expr: &Expr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<BoundExpr, SqlSurfaceError> {
    if matches!(expr, Expr::Null) {
        charge_node(node_budget)?;
        return Ok(BoundExpr::Null);
    }
    let (bound, ty) = crate::sql::udf_call::bind_expr_any(expr, env, node_budget)?;
    match ty {
        ExprType::Json | ExprType::Text | ExprType::Scalar | ExprType::Bool => Ok(bound),
        _ => Err(sqljson_unsupported(
            "this PASSING value type is not supported by SQL/JSON query functions",
        )),
    }
}

/// SQL/JSON 問い合わせ関数 1 呼び出しの束縛（`udf_call::bind_expr_any` の `Expr::SqlJsonQuery` 腕から呼ばれる）。
pub(super) fn bind_sql_json_query(
    q: &SqlJsonQueryExpr,
    env: &mut BindEnv<'_>,
    node_budget: &mut usize,
) -> Result<(BoundExpr, ExprType), SqlSurfaceError> {
    let name = q.kind.as_str();
    let ret = bind_sql_json_returning(q.kind, q.returning.as_deref())?;
    let omit_quotes = q.keep_quotes == Some(false);
    if omit_quotes && ret != SqlJsonRet::Text {
        return Err(sqljson_unsupported(
            "OMIT QUOTES is only supported with a text RETURNING type",
        ));
    }
    charge_node(node_budget)?;
    let target = bind_sql_json_context(name, &q.context, env, node_budget)?;
    let path = match &q.path {
        Expr::Null | Expr::String(_) => bind_path_arg(name, &q.path, node_budget)?,
        _ => {
            return Err(sqljson_unsupported(
                "the path of SQL/JSON query functions must be a string literal",
            ))
        }
    };
    if q.passing.len() > crate::sql::udf_call::MAX_CALL_ARGS {
        return Err(SqlSurfaceError::payload_too_large(
            "too many PASSING arguments",
        ));
    }
    let mut passing: Vec<(String, BoundExpr)> = Vec::with_capacity(q.passing.len());
    for (var, e) in &q.passing {
        if passing.iter().any(|(n, _)| n == var) {
            return Err(sqljson_unsupported(
                "duplicate PASSING variable names are not supported",
            ));
        }
        let bound = bind_sql_json_passing(e, env, node_budget)?;
        passing.push((var.clone(), bound));
    }
    if q.kind == SqlJsonQueryKind::Exists && q.on_empty.is_some() {
        return Err(SqlSurfaceError::unsupported(
            "ON EMPTY is not allowed in JSON_EXISTS",
        ));
    }
    let on_empty = bind_sql_json_behavior(q.kind, ret, q.on_empty.as_ref(), node_budget)?;
    let on_error = bind_sql_json_behavior(q.kind, ret, q.on_error.as_ref(), node_budget)?;
    let (fn_kind, ty) = match q.kind {
        SqlJsonQueryKind::Exists => (JsonPathFnKind::SqlExists, ExprType::Bool),
        SqlJsonQueryKind::Value => (JsonPathFnKind::SqlValue, ExprType::Text),
        SqlJsonQueryKind::Query => (
            JsonPathFnKind::SqlQuery,
            match ret {
                SqlJsonRet::Jsonb => ExprType::Json,
                SqlJsonRet::Json => ExprType::JsonText,
                SqlJsonRet::Text => ExprType::Text,
            },
        ),
    };
    Ok((
        BoundExpr::JsonPathFn(Box::new(BoundJsonPathFn {
            kind: fn_kind,
            target,
            path,
            vars: None,
            silent: SilentArg::Omitted,
            limits: current_jsonpath_limits(),
            sqljson: Some(Box::new(BoundSqlJsonClauses {
                ret,
                with_wrapper: q.with_wrapper,
                omit_quotes,
                on_empty,
                on_error,
                passing,
            })),
        })),
        ty,
    ))
}

// --- 評価 ---

const MSG_VALUE_NOT_SINGLE: &str = "JSON path expression in JSON_VALUE must return a single item";
const MSG_QUERY_NOT_SINGLE: &str =
    "JSON path expression in JSON_QUERY must return a single item when no wrapper is requested";
const MSG_VALUE_NOT_SCALAR: &str =
    "JSON path expression in JSON_VALUE must return a single scalar item";
const MSG_NO_ITEM: &str = "no SQL/JSON item found for specified path";

fn json_const<'a>(text: &str, ret: SqlJsonRet) -> ExprValue<'a> {
    let owned = Cow::Owned(text.to_string());
    match ret {
        SqlJsonRet::Jsonb => ExprValue::Json(owned),
        SqlJsonRet::Json => ExprValue::JsonText(owned),
        SqlJsonRet::Text => ExprValue::Text(owned),
    }
}

/// `ON EMPTY`／`ON ERROR` の挙動を値にする。`ERROR` は `err` をそのまま返す。
fn apply_behavior<'a>(
    b: &BoundBehavior,
    ret: SqlJsonRet,
    err: SqlSurfaceError,
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    match b {
        BoundBehavior::Error => Err(err),
        BoundBehavior::Null | BoundBehavior::Unknown => Ok(ExprValue::Null),
        BoundBehavior::True => Ok(ExprValue::Bool(true)),
        BoundBehavior::False => Ok(ExprValue::Bool(false)),
        BoundBehavior::EmptyArray => Ok(json_const("[]", ret)),
        BoundBehavior::EmptyObject => Ok(json_const("{}", ret)),
        BoundBehavior::Default(BoundDefault::Null) => Ok(ExprValue::Null),
        BoundBehavior::Default(BoundDefault::Text(s)) => Ok(ExprValue::Text(Cow::Owned(s.clone()))),
        BoundBehavior::Default(BoundDefault::Json(s)) => Ok(json_const(s, ret)),
    }
}

/// SQL/JSON 問い合わせ関数の評価（`eval_jsonpath_fn` から句つきの束縛結果で呼ばれる）。
fn eval_sql_json<'a>(
    f: &BoundJsonPathFn,
    c: &BoundSqlJsonClauses,
    id: u64,
    embedding: &'a [f32],
    row_scalars: &'a [Option<ScalarRef<'a>>],
) -> Result<ExprValue<'a>, SqlSurfaceError> {
    use crate::sql::udf_call::eval_with_scalars;
    let ctx_text = match eval_with_scalars(&f.target, id, embedding, row_scalars)? {
        ExprValue::Null => return Ok(ExprValue::Null),
        ExprValue::Json(t) => t,
        _ => return Err(internal("SQL/JSON context item is not a jsonb value")),
    };
    let Some(path) = f.path.as_ref() else {
        return Ok(ExprValue::Null);
    };
    let root = crate::json::parse_jsonb_for_build(&ctx_text).map_err(json_build_error)?;
    let mut vars = std::collections::BTreeMap::new();
    for (var, e) in &c.passing {
        let v = eval_with_scalars(e, id, embedding, row_scalars)?;
        vars.insert(
            var.clone(),
            crate::sql::udf_call::json_value_from_expr_value(&v)?,
        );
    }
    let vars_value = JsonValue::Object(vars);
    let mut opts = JsonPathEvalOptions::new().with_limits(f.limits);
    if !c.passing.is_empty() {
        opts = opts.with_vars(&vars_value);
    }
    // 抑止できる評価エラーだけを ON ERROR の対象にする（それ以外は常に返す）。
    let eval_failure = |e: crate::json::jsonpath::JsonPathEvalError| {
        if e.is_suppressible() {
            apply_behavior(&c.on_error, c.ret, SqlSurfaceError::JsonPathEval(e))
        } else {
            Err(SqlSurfaceError::JsonPathEval(e))
        }
    };
    if f.kind == JsonPathFnKind::SqlExists {
        return match jsonpath_exists(path, &root, opts) {
            Ok(Some(b)) => Ok(ExprValue::Bool(b)),
            Ok(None) => Ok(ExprValue::Null),
            Err(e) => eval_failure(e),
        };
    }
    let items = match jsonpath_query(path, &root, opts) {
        Ok(items) => items,
        Err(e) => return eval_failure(e),
    };
    // ON EMPTY: 項目が 0 件。`ERROR ON EMPTY` は `22035` をそのまま返す。
    if items.is_empty() {
        return apply_behavior(
            &c.on_empty,
            c.ret,
            SqlSurfaceError::NoSqlJsonItem {
                detail: MSG_NO_ITEM,
            },
        );
    }
    let on_error = |err: SqlSurfaceError| apply_behavior(&c.on_error, c.ret, err);
    if f.kind == JsonPathFnKind::SqlValue {
        let [item] = items.as_slice() else {
            return on_error(SqlSurfaceError::MoreThanOneSqlJsonItem {
                detail: MSG_VALUE_NOT_SINGLE,
            });
        };
        return match item {
            JsonValue::Null => Ok(ExprValue::Null),
            JsonValue::Bool(b) => Ok(ExprValue::Text(Cow::Borrowed(if *b {
                "true"
            } else {
                "false"
            }))),
            JsonValue::String(s) => Ok(ExprValue::Text(Cow::Owned(s.clone()))),
            JsonValue::Number(_) => Ok(ExprValue::Text(Cow::Owned(
                serialize_jsonb_bounded(item).map_err(json_build_error)?,
            ))),
            JsonValue::Array(_) | JsonValue::Object(_) => {
                on_error(SqlSurfaceError::SqlJsonScalarRequired {
                    detail: MSG_VALUE_NOT_SCALAR,
                })
            }
        };
    }
    // JSON_QUERY
    let result = if c.with_wrapper {
        JsonValue::Array(items)
    } else {
        let mut it = items.into_iter();
        match (it.next(), it.next()) {
            (Some(one), None) => one,
            _ => {
                return on_error(SqlSurfaceError::MoreThanOneSqlJsonItem {
                    detail: MSG_QUERY_NOT_SINGLE,
                })
            }
        }
    };
    if c.omit_quotes && !c.with_wrapper {
        if let JsonValue::String(s) = result {
            return Ok(ExprValue::Text(Cow::Owned(s)));
        }
    }
    let text = serialize_jsonb_bounded(&result).map_err(json_build_error)?;
    Ok(json_const(&text, c.ret))
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
