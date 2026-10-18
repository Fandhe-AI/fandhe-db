//! 式索引（`CREATE INDEX ... ((<式>))`）の宣言側の検査（INDEX-10、Issue #1647）。
//!
//! 役割と責務境界:
//! - `sql::allowlist::validate_create_index_tokens` が列リストの式要素を解析する際に呼ぶ、
//!   カタログ非参照・権限非依存の検査を持つ。表・列の存在や列の型には一切依存しない
//!   （存在オラクルにならず、権限の無い主体にも同じ結果を返す）。
//! - `precheck_element_tokens`: 式の構文解析より前に行うトークン先読み検査。汎用の式パーサは
//!   `count(*)`・`OVER (...)`・`(SELECT ...)` を `42601` で落とすため、PostgreSQL と同じ
//!   SQLSTATE（`42803`・`42P20`・`0A000`）にはここで分類する。
//! - `check_index_expression_determinism`: 解析済みの式木に対する決定性（IMMUTABLE 相当）検査。
//!   構文だけで確実に非決定的と言えるものを `42P17` にする。引数の型に依存して決まるもの
//!   （時刻型を含む `CAST`・`date_part` 等）は本段では通し、型を知る束縛段（後続 Issue #1648 が
//!   `Storage::create_index` の書き込みトランザクション内で行う）で再判定する。
//! - 組み込み以外の関数（セッション UDF・未知名）は `0A000`。`UdfRegistry` はセッション単位で
//!   永続化されないため、永続する索引がセッション外で消える関数に依存すると維持できず
//!   fail-open になる。
//!
//! 判定表と PostgreSQL との差は `docs/design/expression-index.md` を参照。

use crate::sql::allowlist::{is_aggregate_function_name, SqlSurfaceError, WindowFunc};
use crate::sql::lexer::{Keyword, Token};
use crate::sql::udf_call::{
    is_builtin_function_name, is_current_time_function_name, ArraySubscript, Expr,
};

/// 部分クエリの拒否文言（PostgreSQL 17 の公開文言）。
const MSG_SUBQUERY: &str = "cannot use subquery in index expression";
/// 集計関数の拒否文言。
const MSG_AGGREGATE: &str = "aggregate functions are not allowed in index expressions";
/// ウィンドウ関数の拒否文言。
const MSG_WINDOW: &str = "window functions are not allowed in index expressions";
/// 集合返却関数の拒否文言。
const MSG_SRF: &str = "set-returning functions are not allowed in index expressions";
/// 非決定的な式の拒否文言。
const MSG_NOT_IMMUTABLE: &str = "functions in index expression must be marked IMMUTABLE";
/// 組み込み以外の関数の拒否文言。
const MSG_UDF: &str = "user-defined functions in index expressions are not supported";

/// 式要素 1 個のトークン区間の終端（`start` から最初の深さ 0 の `,` または対応の無い `)` の
/// 位置。見つからなければ `tokens.len()`）を返す。深さカウンタによる線形走査で再帰しない。
pub(crate) fn element_end(tokens: &[Token], start: usize) -> usize {
    let mut depth: usize = 0;
    let mut i = start;
    while let Some(tok) = tokens.get(i) {
        match tok {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            Token::Punct(',') if depth == 0 => return i,
            _ => {}
        }
        i = i.saturating_add(1);
    }
    tokens.len()
}

/// 集合返却関数（SRF）の名前か。本エンジンが名前だけ予約している SRF と、PostgreSQL の代表的な
/// SRF（`unnest`・`generate_series` 等）。
fn is_set_returning_name(lower: &str) -> bool {
    crate::sql::udf_call::is_deferred_json_srf_name(lower)
        || matches!(
            lower,
            "unnest"
                | "generate_series"
                | "generate_subscripts"
                | "json_object_keys"
                | "json_each"
                | "json_each_text"
                | "json_array_elements"
                | "json_array_elements_text"
        )
}

/// 式要素のトークン区間 `tokens` を左から走査し、最初に見つかった集計・ウィンドウ・部分クエリ・
/// SRF をそれぞれ PostgreSQL と同じ SQLSTATE で拒否する（INDEX-10）。
///
/// 判定は呼び出しの開始位置の左端優先。`(` の直後が `SELECT` なら部分クエリ
/// （`EXISTS (SELECT`・`IN (SELECT`・`ARRAY(SELECT` を含む）。`<名前> ( ... )` の直後が `OVER` なら
/// 集計／ウィンドウ関数は `42P20`、それ以外の名前への `OVER` は `42601`（fail-closed）。
/// `OVER` の無い集計は `42803`、`OVER` の無い順位関数は `42601`。
pub(crate) fn precheck_element_tokens(tokens: &[Token]) -> Result<(), SqlSurfaceError> {
    // `(` ごとの対応する `)` の位置を 1 回の走査で求める（呼び出しごとの再走査による
    // 二乗時間を避ける）。対応の無い `(` は `None`。
    let mut close: Vec<Option<usize>> = vec![None; tokens.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            Token::Punct('(') => stack.push(i),
            Token::Punct(')') => {
                if let Some(open) = stack.pop() {
                    if let Some(slot) = close.get_mut(open) {
                        *slot = Some(i);
                    }
                }
            }
            _ => {}
        }
    }
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            Token::Punct('(')
                if matches!(
                    tokens.get(i.saturating_add(1)),
                    Some(Token::Keyword(Keyword::Select))
                ) =>
            {
                return Err(SqlSurfaceError::FeatureNotSupported {
                    detail: MSG_SUBQUERY.to_string(),
                });
            }
            Token::Ident(name)
                if matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct('('))) =>
            {
                let open = i.saturating_add(1);
                let has_over = close
                    .get(open)
                    .copied()
                    .flatten()
                    .and_then(|c| tokens.get(c.saturating_add(1)))
                    .is_some_and(
                        |t| matches!(t, Token::Ident(n) if n.eq_ignore_ascii_case("OVER")),
                    );
                let is_agg = is_aggregate_function_name(name);
                let is_window = WindowFunc::from_name(name).is_some();
                if has_over {
                    if is_agg || is_window {
                        return Err(SqlSurfaceError::WindowingError { detail: MSG_WINDOW });
                    }
                    return Err(SqlSurfaceError::unsupported(
                        "OVER is not allowed for a non-window function",
                    ));
                }
                if is_agg {
                    return Err(SqlSurfaceError::GroupingError {
                        detail: MSG_AGGREGATE,
                    });
                }
                if is_window {
                    return Err(SqlSurfaceError::unsupported(
                        "window function requires an OVER clause",
                    ));
                }
                if is_set_returning_name(&name.to_ascii_lowercase()) {
                    return Err(SqlSurfaceError::FeatureNotSupported {
                        detail: MSG_SRF.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// 構文だけで確実に非決定的（または STABLE／VOLATILE）な関数名。PostgreSQL 17 の
/// `pg_proc.provolatile` が全オーバーロードで非 IMMUTABLE のものに限る。本エンジンに無い既知の
/// 非決定名（`random`・`nextval` 等）は `42883` へ落とさず `42P17` を返すためここに含める。
fn is_known_non_immutable_name(lower: &str) -> bool {
    matches!(
        lower,
        "concat"
            | "concat_ws"
            | "to_json"
            | "to_jsonb"
            | "json_build_object"
            | "json_build_array"
            | "jsonb_build_object"
            | "jsonb_build_array"
            | "array_to_string"
            | "random"
            | "setseed"
            | "nextval"
            | "currval"
            | "setval"
            | "lastval"
            | "gen_random_uuid"
            | "uuid_generate_v4"
            | "timeofday"
            | "txid_current"
            | "pg_backend_pid"
            | "current_user"
            | "session_user"
            | "current_setting"
    ) || is_current_time_function_name(lower)
}

fn not_immutable() -> SqlSurfaceError {
    SqlSurfaceError::InvalidObjectDefinition {
        detail: MSG_NOT_IMMUTABLE,
    }
}

/// 解析済みの式木の決定性を検査する（INDEX-10）。`Expr` の全 variant を `_ =>` 無しで網羅する
/// （variant 追加時にコンパイルエラーで検査漏れに気づく。fail-closed）。再帰の深さは構文段の
/// 式ネスト上限（`MAX_EXPR_DEPTH`）とノード予算で抑えられている。
///
/// 区分: 確定の非決定式は `42P17`、組み込み以外の関数は `0A000`、型に依存する判定
/// （時刻型の `CAST`・`date_part`・`date_trunc`・2 引数の `timezone`・`TIMESTAMPTZ` リテラル・
/// `COLLATE`・SQL/JSON 問い合わせ）は本段では通して子だけ再帰する。型依存の再判定は束縛段
/// （Issue #1648）の責務。
pub(crate) fn check_index_expression_determinism(expr: &Expr) -> Result<(), SqlSurfaceError> {
    match expr {
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Ident(_)
        | Expr::Null
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_)
        | Expr::TimeLiteral(_)
        | Expr::TimestampTzLiteral { .. }
        | Expr::IntervalLiteral(_) => Ok(()),
        // `CURRENT_TIMESTAMP` 等（括弧なしの SQL 値関数）は常に非 IMMUTABLE。
        Expr::ValueFunction { .. } => Err(not_immutable()),
        Expr::Call { name, args } => {
            let lower = name.to_ascii_lowercase();
            // 1 引数の `timezone(<式>)`（`AT LOCAL`）はセッション TimeZone に依存する。
            if is_known_non_immutable_name(&lower) || (lower == "timezone" && args.len() == 1) {
                return Err(not_immutable());
            }
            if !is_builtin_function_name(name) {
                return Err(SqlSurfaceError::FeatureNotSupported {
                    detail: MSG_UDF.to_string(),
                });
            }
            args.iter().try_for_each(check_index_expression_determinism)
        }
        Expr::Binary { lhs, rhs, .. } | Expr::JsonAccess { lhs, rhs, .. } => {
            check_index_expression_determinism(lhs)?;
            check_index_expression_determinism(rhs)
        }
        Expr::Case { whens, else_result } => {
            for (cond, result) in whens {
                check_index_expression_determinism(cond)?;
                check_index_expression_determinism(result)?;
            }
            else_result
                .as_deref()
                .map_or(Ok(()), check_index_expression_determinism)
        }
        Expr::Coalesce(items) | Expr::ArrayConstructor(items) => items
            .iter()
            .try_for_each(check_index_expression_determinism),
        Expr::NullIf(a, b) => {
            check_index_expression_determinism(a)?;
            check_index_expression_determinism(b)
        }
        Expr::Collate { expr, .. } | Expr::Cast { expr, .. } => {
            check_index_expression_determinism(expr)
        }
        Expr::SqlJsonQuery(q) => q
            .children()
            .into_iter()
            .try_for_each(check_index_expression_determinism),
        Expr::Subscript { base, subscript } => {
            check_index_expression_determinism(base)?;
            match subscript {
                ArraySubscript::Index(i) => check_index_expression_determinism(i),
                ArraySubscript::Slice { lo, hi } => {
                    for bound in [lo, hi].into_iter().flatten() {
                        check_index_expression_determinism(bound)?;
                    }
                    Ok(())
                }
            }
        }
        Expr::Quantified { lhs, rhs, .. } => {
            check_index_expression_determinism(lhs)?;
            check_index_expression_determinism(rhs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn toks(sql: &str) -> Vec<Token> {
        tokenize(sql).expect("tokenize")
    }

    fn pre_code(sql: &str) -> Option<&'static str> {
        use crate::error_format::ClassifiedError;
        precheck_element_tokens(&toks(sql))
            .err()
            .map(|e| e.error_class().wire_code())
    }

    #[test]
    fn precheck_classifies_aggregate_window_subquery_srf() {
        assert_eq!(pre_code("count(*)"), Some("42803"));
        assert_eq!(pre_code("sum(a)"), Some("42803"));
        assert_eq!(pre_code("sum(a) OVER ()"), Some("42P20"));
        assert_eq!(pre_code("row_number() OVER ()"), Some("42P20"));
        assert_eq!(pre_code("(SELECT 1)"), Some("0A000"));
        assert_eq!(pre_code("a IN (SELECT b FROM t)"), Some("0A000"));
        assert_eq!(pre_code("unnest(a)"), Some("0A000"));
        // OVER の無い順位関数・非窓関数への OVER は 42601（fail-closed）。
        assert_eq!(pre_code("row_number()"), Some("42601"));
        assert_eq!(pre_code("lower(a) OVER ()"), Some("42601"));
        // 通常の式は通す。
        assert_eq!(pre_code("lower(a)"), None);
        assert_eq!(pre_code("a + 1"), None);
    }

    #[test]
    fn precheck_is_leftmost_first() {
        assert_eq!(
            pre_code("coalesce(sum(a) OVER (), (SELECT 1))"),
            Some("42P20")
        );
        assert_eq!(pre_code("coalesce((SELECT 1), sum(a))"), Some("0A000"));
        assert_eq!(pre_code("coalesce(max(a), (SELECT 1))"), Some("42803"));
    }

    #[test]
    fn element_end_stops_at_top_level_delimiters() {
        let t = toks("lower(a, b), c)");
        let end = element_end(&t, 0);
        assert!(matches!(t.get(end), Some(Token::Punct(','))));
        let t = toks("a + 1)");
        let end = element_end(&t, 0);
        assert!(matches!(t.get(end), Some(Token::Punct(')'))));
        // 閉じ括弧が無ければ終端は末尾。
        let t = toks("lower(a");
        assert_eq!(element_end(&t, 0), t.len());
    }
}
