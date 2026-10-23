//! 式索引（`CREATE INDEX ... ((<式>))`）の宣言側の検査（INDEX-10、Issue #1647）。
//!
//! 役割と責務境界:
//! - `sql::allowlist::validate_create_index_tokens` が列リストの式要素を解析する際に呼ぶ、
//!   カタログ非参照・権限非依存の検査を持つ。表・列の存在や列の型には一切依存しない
//!   （存在オラクルにならず、権限の無い主体にも同じ結果を返す）。
//! - `precheck_index_tokens`: 式の構文解析より前に行うトークン先読み検査。汎用の式パーサは
//!   `count(*)`・`OVER (...)`・`(SELECT ...)` を `42601` で落とすため、PostgreSQL と同じ
//!   SQLSTATE（`42803`・`42P20`・`0A000`）にはここで分類する。
//! - `check_index_expression_determinism`: 解析済みの式木に対する決定性（IMMUTABLE 相当）検査。
//!   構文だけで確実に非決定的と言えるものを `42P17` にする。引数の型に依存して決まるもの
//!   （時刻型を含む `CAST`・`date_part` 等）は本段では通し、型を知る束縛段
//!   （[`bind_index_expression`]。Issue #1648 が `Storage::create_index` の書き込みトランザクション内で
//!   呼ぶ）で再判定する。
//! - 組み込み以外の関数（セッション UDF・未知名）は `0A000`。`UdfRegistry` はセッション単位で
//!   永続化されないため、永続する索引がセッション外で消える関数に依存すると維持できず
//!   fail-open になる。
//!
//! - `check_index_predicate_determinism`: 部分索引の述語（INDEX-11、Issue #1653）の決定性検査。
//!   先読み・式の検査は [`IndexExprSite::Predicate`] で文言だけ切り替えて共有する。
//!
//! 判定表と PostgreSQL との差は `docs/design/expression-index.md` を参照。

use crate::sql::allowlist::{
    is_aggregate_function_name, SqlSurfaceError, WherePredicate, WindowFunc,
};
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

/// 検査の文脈。式索引のキー式（INDEX-10）と部分索引の述語（INDEX-11、Issue #1653）で、同じ
/// 検査を共有しつつ PostgreSQL 17 の公開文言だけを切り替える（SQLSTATE は共通）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexExprSite {
    /// 式索引のキー式（`((<式>))`・`f(...)`）。
    Key,
    /// 部分索引の `WHERE` 述語。
    Predicate,
}

impl IndexExprSite {
    fn subquery(self) -> &'static str {
        match self {
            Self::Key => MSG_SUBQUERY,
            Self::Predicate => "cannot use subquery in index predicate",
        }
    }
    fn aggregate(self) -> &'static str {
        match self {
            Self::Key => MSG_AGGREGATE,
            Self::Predicate => "aggregate functions are not allowed in index predicates",
        }
    }
    fn window(self) -> &'static str {
        match self {
            Self::Key => MSG_WINDOW,
            Self::Predicate => "window functions are not allowed in index predicates",
        }
    }
    fn srf(self) -> &'static str {
        match self {
            Self::Key => MSG_SRF,
            Self::Predicate => "set-returning functions are not allowed in index predicates",
        }
    }
    fn not_immutable(self) -> &'static str {
        match self {
            Self::Key => MSG_NOT_IMMUTABLE,
            Self::Predicate => "functions in index predicate must be marked IMMUTABLE",
        }
    }
    fn udf(self) -> &'static str {
        match self {
            Self::Key => MSG_UDF,
            Self::Predicate => "user-defined functions in index predicates are not supported",
        }
    }
}

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
pub(crate) fn precheck_index_tokens(
    tokens: &[Token],
    site: IndexExprSite,
) -> Result<(), SqlSurfaceError> {
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
                    detail: site.subquery().to_string(),
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
                        return Err(SqlSurfaceError::WindowingError {
                            detail: site.window(),
                        });
                    }
                    return Err(SqlSurfaceError::unsupported(
                        "OVER is not allowed for a non-window function",
                    ));
                }
                if is_agg {
                    return Err(SqlSurfaceError::GroupingError {
                        detail: site.aggregate(),
                    });
                }
                if is_window {
                    return Err(SqlSurfaceError::unsupported(
                        "window function requires an OVER clause",
                    ));
                }
                if is_set_returning_name(&name.to_ascii_lowercase()) {
                    return Err(SqlSurfaceError::FeatureNotSupported {
                        detail: site.srf().to_string(),
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
        || crate::sql::udf_call::ValueFunctionKind::from_keyword(lower).is_some()
}

/// 式キー文脈（[`IndexExprSite::Key`]）の非決定的な式の拒否（束縛段の再判定が使う）。
fn not_immutable() -> SqlSurfaceError {
    SqlSurfaceError::InvalidObjectDefinition {
        detail: MSG_NOT_IMMUTABLE,
    }
}

/// 文脈に応じた非決定的な式の拒否（`42P17`。文言は固定で、列名・値を載せない）。
fn not_immutable_at(site: IndexExprSite) -> SqlSurfaceError {
    SqlSurfaceError::InvalidObjectDefinition {
        detail: site.not_immutable(),
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
pub(crate) fn check_index_expression_determinism(
    expr: &Expr,
    site: IndexExprSite,
) -> Result<(), SqlSurfaceError> {
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
        Expr::ValueFunction { .. } => Err(not_immutable_at(site)),
        Expr::Call { name, args } => {
            let lower = name.to_ascii_lowercase();
            // 1 引数の `timezone(<式>)`（`AT LOCAL`）はセッション TimeZone に依存する。
            if is_known_non_immutable_name(&lower) || (lower == "timezone" && args.len() == 1) {
                return Err(not_immutable_at(site));
            }
            if !is_builtin_function_name(name) {
                return Err(SqlSurfaceError::FeatureNotSupported {
                    detail: site.udf().to_string(),
                });
            }
            args.iter()
                .try_for_each(|e| check_index_expression_determinism(e, site))
        }
        Expr::Binary { lhs, rhs, .. } | Expr::JsonAccess { lhs, rhs, .. } => {
            check_index_expression_determinism(lhs, site)?;
            check_index_expression_determinism(rhs, site)
        }
        Expr::Case { whens, else_result } => {
            for (cond, result) in whens {
                check_index_expression_determinism(cond, site)?;
                check_index_expression_determinism(result, site)?;
            }
            else_result
                .as_deref()
                .map_or(Ok(()), |e| check_index_expression_determinism(e, site))
        }
        Expr::Coalesce(items) | Expr::ArrayConstructor(items) => items
            .iter()
            .try_for_each(|e| check_index_expression_determinism(e, site)),
        Expr::NullIf(a, b) => {
            check_index_expression_determinism(a, site)?;
            check_index_expression_determinism(b, site)
        }
        // `IS [NOT] DISTINCT FROM` は索引の式・述語では対象外（fail-closed。設計文書の対象外節）。
        Expr::IsDistinctFrom { .. } => Err(SqlSurfaceError::FeatureNotSupported {
            detail: "IS [NOT] DISTINCT FROM is not supported in an index expression or predicate"
                .to_string(),
        }),
        // 正規表現マッチも同様（SQL-55・Issue #1787。fail-closed）。
        Expr::RegexMatch { .. } => Err(SqlSurfaceError::FeatureNotSupported {
            detail:
                "regular expression operators are not supported in an index expression or predicate"
                    .to_string(),
        }),
        Expr::Collate { expr, .. } | Expr::Cast { expr, .. } => {
            check_index_expression_determinism(expr, site)
        }
        Expr::SqlJsonQuery(q) => q
            .children()
            .into_iter()
            .try_for_each(|e| check_index_expression_determinism(e, site)),
        Expr::Subscript { base, subscript } => {
            check_index_expression_determinism(base, site)?;
            match subscript {
                ArraySubscript::Index(i) => check_index_expression_determinism(i, site),
                ArraySubscript::Slice { lo, hi } => {
                    for bound in [lo, hi].into_iter().flatten() {
                        check_index_expression_determinism(bound, site)?;
                    }
                    Ok(())
                }
            }
        }
        Expr::Quantified { lhs, rhs, .. } => {
            check_index_expression_determinism(lhs, site)?;
            check_index_expression_determinism(rhs, site)
        }
    }
}

/// 部分索引の述語（`WherePredicate` の木）の決定性を検査する（INDEX-11、Issue #1653）。
/// `WherePredicate` の全 variant を `_ =>` 無しで網羅する（variant 追加時のコンパイルエラーで
/// 検査漏れに気づく。fail-closed）。列とリテラルだけの葉は決定的。式述語は
/// [`check_index_expression_determinism`] へ委譲する。`validate_create_index_tokens` から呼ばれ、
/// カタログ・型には依存しない（型依存の再判定は束縛段〔Issue #1654〕の責務）。
pub(crate) fn check_index_predicate_determinism(
    preds: &[WherePredicate],
) -> Result<(), SqlSurfaceError> {
    preds.iter().try_for_each(check_one_predicate)
}

fn check_one_predicate(pred: &WherePredicate) -> Result<(), SqlSurfaceError> {
    let site = IndexExprSite::Predicate;
    match pred {
        // 式左辺の LIKE（SQL-54、Issue #1782）は部分索引の述語として扱えない。含意判定・索引再構築の
        // 経路が式左辺を評価できないため、決定性にかかわらず `0A000` で fail-closed に拒否する
        // （IS DISTINCT FROM〔Issue #1783〕と同じ方針）。
        WherePredicate::Prefix { lhs, .. } if lhs.as_bare_column().is_none() => {
            Err(SqlSurfaceError::FeatureNotSupported {
                detail:
                    "LIKE with a non-column left operand is not supported in an index predicate"
                        .to_string(),
            })
        }
        WherePredicate::Equality { .. }
        | WherePredicate::Prefix { .. }
        | WherePredicate::BoolEquality { .. }
        | WherePredicate::BoolColumn { .. }
        | WherePredicate::Compare { .. }
        | WherePredicate::InList { .. }
        | WherePredicate::Between { .. }
        | WherePredicate::IsNull { .. } => Ok(()),
        WherePredicate::Expression(expr) => check_index_expression_determinism(expr, site),
        WherePredicate::Not(inner) => check_one_predicate(inner),
        WherePredicate::Or(branches) => branches
            .iter()
            .try_for_each(|branch| check_index_predicate_determinism(branch)),
        // RLS 述語呼び出しはセッション主体に依存する。永続する索引へ埋め込ませない（P0）。
        WherePredicate::PredicateCall { .. } => Err(SqlSurfaceError::unsupported(
            "visible() is not allowed in an index predicate",
        )),
        // 構文段（`subquery_ctx = None`）では生成されないが、fail-closed の防御として拒否する。
        WherePredicate::InSubquery { .. }
        | WherePredicate::Exists { .. }
        | WherePredicate::ScalarSubqueryCompare { .. } => {
            Err(SqlSurfaceError::FeatureNotSupported {
                detail: site.subquery().to_string(),
            })
        }
        // パーサが生成しない内部専用の葉。到達しないが、防御的に拒否する。
        WherePredicate::IdCompare { .. } => Err(SqlSurfaceError::unsupported(
            "unsupported predicate in an index predicate",
        )),
    }
}

/// 式木のどこかに `COLLATE` を含むかを再帰的に判定する（式索引は照合を永続形に持たないため、
/// 最上位だけでなく入れ子の `lower(col COLLATE "C")` も fail-closed で拒否する根拠）。
/// `Expr` の全 variant を網羅する（variant 追加時にコンパイルエラーで漏れに気づく）。
pub(crate) fn expr_contains_collate(expr: &Expr) -> bool {
    match expr {
        Expr::Collate { .. } => true,
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Ident(_)
        | Expr::Null
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_)
        | Expr::TimeLiteral(_)
        | Expr::TimestampTzLiteral { .. }
        | Expr::IntervalLiteral(_)
        | Expr::ValueFunction { .. } => false,
        Expr::Call { args, .. } => args.iter().any(expr_contains_collate),
        Expr::Binary { lhs, rhs, .. }
        | Expr::JsonAccess { lhs, rhs, .. }
        | Expr::Quantified { lhs, rhs, .. } => {
            expr_contains_collate(lhs) || expr_contains_collate(rhs)
        }
        Expr::Case { whens, else_result } => {
            whens
                .iter()
                .any(|(c, r)| expr_contains_collate(c) || expr_contains_collate(r))
                || else_result.as_deref().is_some_and(expr_contains_collate)
        }
        Expr::Coalesce(items) | Expr::ArrayConstructor(items) => {
            items.iter().any(expr_contains_collate)
        }
        Expr::NullIf(a, b)
        | Expr::IsDistinctFrom { lhs: a, rhs: b, .. }
        | Expr::RegexMatch {
            lhs: a, pattern: b, ..
        } => expr_contains_collate(a) || expr_contains_collate(b),
        Expr::Cast { expr, .. } => expr_contains_collate(expr),
        Expr::SqlJsonQuery(q) => q.children().into_iter().any(expr_contains_collate),
        Expr::Subscript { base, subscript } => {
            expr_contains_collate(base)
                || match subscript {
                    ArraySubscript::Index(i) => expr_contains_collate(i),
                    ArraySubscript::Slice { lo, hi } => [lo, hi]
                        .into_iter()
                        .flatten()
                        .any(|b| expr_contains_collate(b)),
                }
        }
    }
}

// ---------------------------------------------------------------------------
// 束縛段（Issue #1648）: 型を知る側（`catalog::Storage::create_index` の書き込みトランザクションと
// `sql::scalar_index` の式キー構築）が呼ぶ。
// ---------------------------------------------------------------------------

/// 索引化できない結果型の拒否文言（固定。列名・値を含めない）。
const MSG_RESULT_TYPE: &str = "index expression result type is not supported yet";
/// 正規化テキストへ往復できない式の拒否文言（fail-closed。壊れた式を永続化しない）。
const MSG_NOT_STORABLE: &str = "index expression cannot be stored in normalized form";
/// セッション TZ での畳み込み失敗の拒否文言（固定。値を含めない）。
const MSG_TZ_FOLD: &str =
    "timestamp with time zone literal is out of range or does not exist in the session time zone";
/// 式への `COLLATE` の拒否文言（式キーの辞書はバイト列順のみ。後続対応）。
pub(crate) const MSG_COLLATE: &str = "COLLATE on an index expression is not supported yet";

/// 式要素 1 個を束縛した結果（[`bind_index_expression`] の戻り値）。
///
/// 呼び出し元は 2 つ: `catalog::Storage::create_index` の書き込みトランザクション（保存用の
/// 正規化テキストと参照列を得る）と `sql::scalar_index` の式キー構築（評価プログラムを得る）。
#[derive(Debug)]
pub(crate) struct BoundIndexExpression {
    /// 束縛済み式（参照列の走査マスク計算に使う）。
    pub(crate) bound: crate::sql::udf_call::BoundExpr,
    /// 結果型（索引化可能な型だけがここまで届く）。
    pub(crate) ty: crate::sql::udf_call::ExprType,
    /// コンパイル済みの評価プログラム。
    pub(crate) program: crate::sql::expr_program::ExprProgram,
    /// 往復一致を確認済みの正規化 SQL テキスト（カタログへ保存する値）。
    pub(crate) sql: String,
    /// 参照するスキーマ列名（重複なし・スキーマ宣言順）。
    pub(crate) referenced_columns: Vec<String>,
    /// VECTOR 列（埋め込み）を参照するか。
    pub(crate) references_embedding: bool,
}

/// ゾーン無し `TIMESTAMPTZ` リテラル（`session_local`）を、定義時のセッション TZ で UTC の確定値へ
/// 畳んだ複製を返す。`check_constraint::render_expr` が描く `+00` 形と一致させ、描画→再解析の
/// 往復一致と、セッション TZ に依存しない永続化を両立する。`Expr` の全 variant を網羅する
/// （`_ =>` 無し）。CHECK 側の同名処理は `Cast` 等の内側を再帰しないため流用しない。
pub(crate) fn fold_session_local_literals(expr: &Expr) -> Result<Expr, SqlSurfaceError> {
    let rec = |e: &Expr| fold_session_local_literals(e);
    Ok(match expr {
        Expr::TimestampTzLiteral {
            micros,
            session_local,
        } => {
            if *session_local {
                // 変換できない（DST の欠落時刻・範囲外）場合は fail-closed で拒否する。元の
                // micros を UTC として残すと、誤った時刻が永続化される。
                let utc = crate::datetime::with_current_session_time_zone(|tz| {
                    tz.timestamp_to_timestamptz(*micros)
                })
                .ok_or_else(|| SqlSurfaceError::DatetimeFieldOverflow {
                    detail: MSG_TZ_FOLD.to_string(),
                })?;
                Expr::TimestampTzLiteral {
                    micros: utc,
                    session_local: false,
                }
            } else {
                expr.clone()
            }
        }
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Ident(_)
        | Expr::Null
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_)
        | Expr::TimeLiteral(_)
        | Expr::IntervalLiteral(_)
        | Expr::ValueFunction { .. } => expr.clone(),
        Expr::Call { name, args } => Expr::Call {
            name: name.clone(),
            args: args.iter().map(rec).collect::<Result<_, _>>()?,
        },
        Expr::Binary { op, lhs, rhs } => Expr::Binary {
            op: *op,
            lhs: Box::new(rec(lhs)?),
            rhs: Box::new(rec(rhs)?),
        },
        Expr::Case { whens, else_result } => Expr::Case {
            whens: whens
                .iter()
                .map(|(c, r)| Ok((rec(c)?, rec(r)?)))
                .collect::<Result<_, SqlSurfaceError>>()?,
            else_result: match else_result.as_deref() {
                Some(e) => Some(Box::new(rec(e)?)),
                None => None,
            },
        },
        Expr::JsonAccess { op, lhs, rhs } => Expr::JsonAccess {
            op: *op,
            lhs: Box::new(rec(lhs)?),
            rhs: Box::new(rec(rhs)?),
        },
        Expr::ArrayConstructor(_) | Expr::Subscript { .. } | Expr::Quantified { .. } => {
            match expr.try_map_array_children(&mut |e| rec(e)) {
                Some(r) => r?,
                None => expr.clone(),
            }
        }
        Expr::Coalesce(args) => Expr::Coalesce(args.iter().map(rec).collect::<Result<_, _>>()?),
        Expr::NullIf(a, b) => Expr::NullIf(Box::new(rec(a)?), Box::new(rec(b)?)),
        Expr::IsDistinctFrom { negated, lhs, rhs } => Expr::IsDistinctFrom {
            negated: *negated,
            lhs: Box::new(rec(lhs)?),
            rhs: Box::new(rec(rhs)?),
        },
        Expr::RegexMatch {
            negated,
            case_insensitive,
            lhs,
            pattern,
            raw_pattern_len,
        } => Expr::RegexMatch {
            negated: *negated,
            case_insensitive: *case_insensitive,
            lhs: Box::new(rec(lhs)?),
            pattern: Box::new(rec(pattern)?),
            raw_pattern_len: *raw_pattern_len,
        },
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(rec(expr)?),
            collation: *collation,
        },
        Expr::SqlJsonQuery(q) => Expr::SqlJsonQuery(Box::new(q.try_map_children(&mut |e| rec(e))?)),
        Expr::Cast { expr, ty } => Expr::Cast {
            expr: Box::new(rec(expr)?),
            ty: ty.clone(),
        },
    })
}

/// 部分式 `sub` の静的な結果型。束縛できない（文脈依存の型なし `NULL` 等）場合は `None`。
fn static_type_of(
    sub: &Expr,
    schema: &crate::catalog::TableSchema,
) -> Option<crate::sql::udf_call::ExprType> {
    let udfs = crate::sql::udf_call::UdfRegistry::default();
    let mut budget = crate::sql::udf_call::MAX_EXPR_NODES;
    crate::sql::udf_call::bind_order_expr(sub, schema, &udfs, &mut budget)
        .ok()
        .map(|(_, ty)| ty)
}

/// 時刻・間隔型（文字列との相互変換が `DateStyle`／`IntervalStyle` に依存する型）か。
fn is_datetime_type(ty: crate::sql::udf_call::ExprType) -> bool {
    use crate::sql::udf_call::ExprType;
    matches!(
        ty,
        ExprType::Date
            | ExprType::Time
            | ExprType::Timestamp
            | ExprType::TimestampTz
            | ExprType::Interval
    )
}

/// `CastType` が文字列系（`text`／`varchar`／`bpchar`）か。
fn cast_is_text_family(ty: &crate::sql::cast::CastType) -> bool {
    use crate::sql::cast::CastType;
    matches!(
        ty,
        CastType::Text | CastType::Varchar(_) | CastType::Bpchar(_)
    )
}

/// `CastType` が時刻・間隔型か。
fn cast_is_datetime(ty: &crate::sql::cast::CastType) -> bool {
    use crate::sql::cast::CastType;
    matches!(
        ty,
        CastType::Date
            | CastType::Time
            | CastType::Timestamp
            | CastType::TimestampTz
            | CastType::Interval
    )
}

/// 束縛後（型が確定した後）の決定性再判定（INDEX-10）。構文段の
/// [`check_index_expression_determinism`] が通した「型に依存して決まる」区分のうち、PostgreSQL 17 で
/// IMMUTABLE でないオーバーロードに束縛されるものを `42P17` にする。判定表:
///
/// - `TIMESTAMPTZ` 型の引数を取る関数呼び出し（`date_part`・`date_trunc`・`extract` 等。
///   `timezone(<zone>, <値>)` の 2 引数形だけは IMMUTABLE のため除く）
/// - `TIMESTAMPTZ` と `TIMESTAMP`／`DATE` の演算、`TIMESTAMPTZ ± INTERVAL`
/// - `CAST`: 変換元または変換先が `TIMESTAMPTZ`（もう一方が `TIMESTAMPTZ` 以外）、
///   時刻・間隔型と文字列系の相互変換（`DateStyle`／`IntervalStyle` 依存）
///
/// 型を決められない部分式はこの判定に寄与しない（式全体は束縛に成功している）。
/// `Expr` の全 variant を網羅する（`_ =>` 無し。fail-closed）。
pub(crate) fn check_bound_index_expression_determinism(
    expr: &Expr,
    schema: &crate::catalog::TableSchema,
) -> Result<(), SqlSurfaceError> {
    use crate::sql::udf_call::{BinOp, ExprType};
    let rec = |e: &Expr| check_bound_index_expression_determinism(e, schema);
    let ty_of = |e: &Expr| static_type_of(e, schema);
    match expr {
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Ident(_)
        | Expr::Null
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_)
        | Expr::TimeLiteral(_)
        | Expr::TimestampTzLiteral { .. }
        | Expr::IntervalLiteral(_)
        | Expr::ValueFunction { .. } => Ok(()),
        Expr::Call { name, args } => {
            let two_arg_timezone = name.eq_ignore_ascii_case("timezone") && args.len() == 2;
            if !two_arg_timezone && args.iter().any(|a| ty_of(a) == Some(ExprType::TimestampTz)) {
                return Err(not_immutable());
            }
            // 2 引数 `timezone(<zone>, <値>)` は、値が型なし文字列リテラル（セッション TZ で
            // `TIMESTAMPTZ` として解釈）または `DATE`（セッション TZ の 0 時へ変換）のとき、束縛が
            // セッション TZ 依存の暗黙変換を挿入し、再構築するセッションでキーが変わるため拒否する。
            if two_arg_timezone {
                if let Some(value) = args.get(1) {
                    if matches!(value, Expr::String(_)) || ty_of(value) == Some(ExprType::Date) {
                        return Err(not_immutable());
                    }
                }
            }
            // 3 引数 `date_trunc(<単位>, <値>, <ゾーン>)` は、束縛が非 TIMESTAMPTZ の値へ
            // セッション TZ 依存の暗黙変換を挿入するため、引数の型によらず拒否する。
            if name.eq_ignore_ascii_case("date_trunc") && args.len() == 3 {
                return Err(not_immutable());
            }
            args.iter().try_for_each(rec)
        }
        Expr::Binary { op, lhs, rhs } => {
            let (lt, rt) = (ty_of(lhs), ty_of(rhs));
            let tz = |t: Option<ExprType>| t == Some(ExprType::TimestampTz);
            let naive =
                |t: Option<ExprType>| matches!(t, Some(ExprType::Timestamp | ExprType::Date));
            let interval = |t: Option<ExprType>| t == Some(ExprType::Interval);
            let mixed_naive = (tz(lt) && naive(rt)) || (naive(lt) && tz(rt));
            let tz_interval_arith = matches!(op, BinOp::Add | BinOp::Sub)
                && ((tz(lt) && interval(rt)) || (interval(lt) && tz(rt)));
            if mixed_naive || tz_interval_arith || tz_vs_string_literal(lhs, rhs, lt, rt) {
                return Err(not_immutable());
            }
            rec(lhs)?;
            rec(rhs)
        }
        Expr::JsonAccess { lhs, rhs, .. } => {
            rec(lhs)?;
            rec(rhs)
        }
        Expr::Case { whens, else_result } => {
            for (cond, result) in whens {
                rec(cond)?;
                rec(result)?;
            }
            else_result.as_deref().map_or(Ok(()), rec)
        }
        Expr::Coalesce(items) | Expr::ArrayConstructor(items) => {
            let has_tz = items
                .iter()
                .any(|e| ty_of(e) == Some(ExprType::TimestampTz));
            if has_tz && items.iter().any(|e| matches!(e, Expr::String(_))) {
                return Err(not_immutable());
            }
            items.iter().try_for_each(rec)
        }
        Expr::NullIf(a, b) | Expr::IsDistinctFrom { lhs: a, rhs: b, .. } => {
            if tz_vs_string_literal(a, b, ty_of(a), ty_of(b)) {
                return Err(not_immutable());
            }
            rec(a)?;
            rec(b)
        }
        Expr::RegexMatch { lhs, pattern, .. } => {
            rec(lhs)?;
            rec(pattern)
        }
        Expr::Collate { expr, .. } => rec(expr),
        Expr::Cast { expr: inner, ty } => {
            let src = ty_of(inner);
            let src_tz = src == Some(ExprType::TimestampTz);
            let dst_tz = matches!(ty, crate::sql::cast::CastType::TimestampTz);
            // 文字列リテラル等も含め、`TIMESTAMPTZ` への変換は評価時のセッション TZ に依存する。
            let tz_conversion = (src_tz && !dst_tz) || (dst_tz && !src_tz);
            let text_datetime = (cast_is_text_family(ty) && src.is_some_and(is_datetime_type))
                || (cast_is_datetime(ty) && src == Some(ExprType::Text));
            if tz_conversion || text_datetime {
                return Err(not_immutable());
            }
            rec(inner)
        }
        Expr::SqlJsonQuery(q) => q.children().into_iter().try_for_each(rec),
        Expr::Subscript { base, subscript } => {
            rec(base)?;
            match subscript {
                ArraySubscript::Index(i) => rec(i),
                ArraySubscript::Slice { lo, hi } => {
                    for bound in [lo, hi].into_iter().flatten() {
                        rec(bound)?;
                    }
                    Ok(())
                }
            }
        }
        Expr::Quantified { lhs, rhs, .. } => {
            if tz_vs_string_literal(lhs, rhs, ty_of(lhs), ty_of(rhs)) {
                return Err(not_immutable());
            }
            rec(lhs)?;
            rec(rhs)
        }
    }
}

/// 一方が `TIMESTAMPTZ` 型、他方が型なし文字列リテラルか。束縛は文字列リテラルを
/// セッション TZ で `TIMESTAMPTZ` へ暗黙変換する（`bind_unknown_literal_against`）ため、
/// 索引キーが再構築するセッションに依存する（INDEX-10 の `42P17` 対象）。
fn tz_vs_string_literal(
    lhs: &Expr,
    rhs: &Expr,
    lt: Option<crate::sql::udf_call::ExprType>,
    rt: Option<crate::sql::udf_call::ExprType>,
) -> bool {
    use crate::sql::udf_call::ExprType;
    let tz = |t: Option<ExprType>| t == Some(ExprType::TimestampTz);
    (tz(lt) && matches!(rhs, Expr::String(_))) || (tz(rt) && matches!(lhs, Expr::String(_)))
}

/// 結果型が索引キーにできるか（`Text`／`Scalar`／`Date`／`Timestamp`／`Time`／`TimestampTz`）。
/// それ以外は固定文言の `0A000`（`ExprType` の全 variant を網羅する）。
fn check_index_result_type(ty: crate::sql::udf_call::ExprType) -> Result<(), SqlSurfaceError> {
    use crate::sql::udf_call::ExprType;
    match ty {
        ExprType::Text
        | ExprType::Scalar
        | ExprType::Date
        | ExprType::Timestamp
        | ExprType::Time
        | ExprType::TimestampTz => Ok(()),
        ExprType::Vector
        | ExprType::Bool
        | ExprType::Interval
        | ExprType::Json
        | ExprType::JsonText
        | ExprType::Numeric
        | ExprType::Uuid
        | ExprType::Bytea
        | ExprType::Array => Err(SqlSurfaceError::FeatureNotSupported {
            detail: MSG_RESULT_TYPE.to_string(),
        }),
    }
}

/// 式要素 1 個を `schema` に対して束縛する（INDEX-10、Issue #1648）。判定順:
/// 1. 式木内の `COLLATE`（入れ子含む）は `0A000`
/// 2. 定義時セッション TZ でゾーン無し `TIMESTAMPTZ` リテラルを確定値へ畳む
/// 3. 束縛（未知列 `42703`・型不整合 `42883`／`42804` 等は束縛側のエラーをそのまま透過）
/// 4. 型に依存する決定性の再判定（`42P17`）
/// 5. 結果型ゲート（`0A000`）
/// 6. 正規化テキストへ描画し、再解析して元の式と一致することを確認（不一致は `0A000`）
/// 7. 参照列の抽出
///
/// 保存するのは 6 の正規化テキストだけで、呼び出し元の生入力を連結しない。UDF レジストリは
/// 空（構文段が組み込み以外の関数を拒否済みで、セッション外で消える関数に依存しない）。
pub(crate) fn bind_index_expression(
    expr: &Expr,
    schema: &crate::catalog::TableSchema,
) -> Result<BoundIndexExpression, SqlSurfaceError> {
    if expr_contains_collate(expr) {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: MSG_COLLATE.to_string(),
        });
    }
    let folded = fold_session_local_literals(expr)?;
    let udfs = crate::sql::udf_call::UdfRegistry::default();
    let mut budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let (bound, ty) = crate::sql::udf_call::bind_order_expr(&folded, schema, &udfs, &mut budget)?;
    check_bound_index_expression_determinism(&folded, schema)?;
    check_index_result_type(ty)?;
    let sql = crate::sql::check_constraint::render_expr(&folded);
    let not_storable = || SqlSurfaceError::FeatureNotSupported {
        detail: MSG_NOT_STORABLE.to_string(),
    };
    let reparsed =
        crate::sql::allowlist::parse_index_expression_text(&sql).map_err(|_| not_storable())?;
    if reparsed != folded {
        return Err(not_storable());
    }
    let references_embedding = crate::sql::udf_call::references_embedding(&bound);
    let mut mask = vec![false; schema.columns.len()];
    crate::sql::udf_call::mark_referenced_scalar_columns(&bound, &mut mask);
    // VECTOR 列の参照は走査マスクに現れない（埋め込みは別経路で渡す）ため、参照した場合は
    // VECTOR 列を依存列に含める（`DROP COLUMN` の連鎖削除・`RENAME`／`ALTER TYPE` の拒否の判定漏れを防ぐ）。
    let referenced_columns: Vec<String> = schema
        .columns
        .iter()
        .zip(&mask)
        .filter(|(c, used)| **used || (references_embedding && c.ty.is_vector()))
        .map(|(c, _)| c.name.clone())
        .collect();
    let program = crate::sql::expr_program::ExprProgram::compile(&bound);
    Ok(BoundIndexExpression {
        bound,
        ty,
        program,
        sql,
        referenced_columns,
        references_embedding,
    })
}

/// 保存済みの式テキストを `schema` で再束縛する（INDEX-10、Issue #1651）。`RENAME COLUMN` の追随
/// （`rename` が `Some((旧名, 新名))`。AST 上の識別子写像で、文字列置換は使わない。1 回の走査で全識別子へ
/// 一括適用されるため列名の入れ替えでも連鎖置換が起きない）と、`ALTER COLUMN TYPE` の再束縛
/// （`rename` が `None`。新しい型で束縛・決定性・結果型ゲートを再判定する）が共有する唯一の経路。
/// 保存テキストは untrusted として再解析し、[`bind_index_expression`] が往復一致まで確認する。
pub(crate) fn rebind_stored_index_expression(
    stored_sql: &str,
    schema: &crate::catalog::TableSchema,
    rename: Option<(&str, &str)>,
) -> Result<BoundIndexExpression, SqlSurfaceError> {
    let parsed = crate::sql::allowlist::parse_index_expression_text(stored_sql)?;
    let expr = match rename {
        Some((old, new)) => crate::sql::check_constraint::map_expr_idents(&parsed, &|c| {
            if c == old {
                new.to_string()
            } else {
                c.to_string()
            }
        }),
        None => parsed,
    };
    bind_index_expression(&expr, schema)
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
        precheck_index_tokens(&toks(sql), IndexExprSite::Key)
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
    // --- 束縛段（Issue #1648） -------------------------------------------------------

    fn bind_schema() -> crate::catalog::TableSchema {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, true),
                ColumnDef::new("body", ColumnType::Text, true),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("at", ColumnType::Timestamp, true),
                ColumnDef::new("at_tz", ColumnType::TimestampTz, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
            ],
        )
    }

    fn bind_sql(sql: &str) -> Result<BoundIndexExpression, SqlSurfaceError> {
        let expr = crate::sql::allowlist::parse_index_expression_text(sql)?;
        bind_index_expression(&expr, &bind_schema())
    }

    fn bind_code(sql: &str) -> &'static str {
        use crate::error_format::ClassifiedError;
        bind_sql(sql)
            .map(|_| "ok")
            .unwrap_or_else(|e| e.error_class().wire_code())
    }

    /// 描画した正規化テキストは再解析すると同じ木に戻り、再束縛しても同じテキストになる。
    #[test]
    fn bound_expression_text_round_trips() {
        for sql in [
            "lower(lang)",
            "COALESCE(lower(lang), upper(body), 'it''s')",
            "(n + 1) * 2",
            "CASE WHEN n > 1 THEN lower(lang) ELSE upper(lang) END",
            "NULLIF(lang, 'x')",
            "CAST(n AS text)",
            "length(lang) - 3",
            "substr(body, 1, 3)",
            "'multi\nline'",
        ] {
            let bound = bind_sql(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
            let again = bind_sql(&bound.sql).unwrap_or_else(|e| panic!("{}: {e}", bound.sql));
            assert_eq!(bound.sql, again.sql, "{sql}");
        }
        // 参照列は重複なし・スキーマ宣言順。
        let bound = bind_sql("COALESCE(lower(body), lower(lang), lower(body))").expect("bind");
        assert_eq!(bound.referenced_columns, vec!["lang", "body"]);
        assert!(!bound.references_embedding);
    }

    /// 結果型ゲート: 索引化できる型は通し、それ以外は固定文言の `0A000`。
    #[test]
    fn result_type_gate_is_closed() {
        for ok in ["lower(lang)", "n + 1", "at", "at_tz", "DATE '2020-01-01'"] {
            assert_eq!(bind_code(ok), "ok", "{ok}");
        }
        for unsupported in ["CAST(n AS numeric)", "CAST(lang AS uuid)"] {
            assert_eq!(bind_code(unsupported), "0A000", "{unsupported}");
        }
        // 束縛エラーは透過する。
        assert_eq!(bind_code("lower(nope)"), "42703");
    }

    /// 型によって決まる決定性の判定表（PostgreSQL 17 の IMMUTABLE でないオーバーロード）。
    #[test]
    fn type_dependent_determinism_table() {
        for not_immutable in [
            "date_part('hour', at_tz)",
            "CAST(at_tz AS date)",
            "CAST(at AS timestamptz)",
            "CAST(at AS text)",
            "CAST(lang AS timestamp)",
            "at_tz + INTERVAL '1 day'",
            "at_tz - at",
            "COALESCE(at_tz, TIMESTAMPTZ '2020-01-01 00:00:00+00')",
            "CASE WHEN at_tz > '2024-01-01 00:00:00' THEN 1 ELSE 0 END",
            "CASE WHEN '2024-01-01 00:00:00' < at_tz THEN 1 ELSE 0 END",
            "date_trunc('day', at, 'UTC')",
            "timezone('UTC', '2024-01-01 00:00:00')",
            "timezone('UTC', DATE '2024-01-01')",
        ] {
            // 最後の例は列そのものの型が同じで通る。除外して個別に確認する。
            if not_immutable.starts_with("COALESCE") {
                assert_eq!(bind_code(not_immutable), "ok", "{not_immutable}");
                continue;
            }
            assert_eq!(bind_code(not_immutable), "42P17", "{not_immutable}");
        }
        for immutable in [
            "date_part('hour', at)",
            "at + INTERVAL '1 day'",
            "timezone('UTC', at_tz)",
            "CAST(n AS text)",
        ] {
            assert_eq!(bind_code(immutable), "ok", "{immutable}");
        }
    }

    /// ゾーン無し `TIMESTAMPTZ` リテラルは、定義時のセッション TZ で UTC の確定値へ畳まれて保存される。
    /// 保存形は TZ に依存せず、再束縛しても同じテキストに戻る。
    #[test]
    fn session_local_timestamptz_literal_is_folded_at_definition() {
        let sql = "COALESCE(at_tz, TIMESTAMPTZ '2024-01-01 09:00:00')";
        let utc = bind_sql(sql).expect("utc").sql;
        let tokyo = {
            let tz = crate::datetime::SessionTimeZone::parse_setting("Asia/Tokyo").expect("tz");
            let _scope = crate::datetime::SessionTimeZoneScope::enter(tz);
            bind_sql(sql).expect("tokyo").sql
        };
        // 09:00 JST は 00:00 UTC。保存形は明示 offset の UTC 定数。
        assert_ne!(utc, tokyo);
        assert!(tokyo.contains("2024-01-01 00:00:00+00"), "{tokyo}");
        assert!(utc.contains("2024-01-01 09:00:00+00"), "{utc}");
        // 畳んだ後の保存形は、どの TZ で再束縛しても変わらない。
        let tz = crate::datetime::SessionTimeZone::parse_setting("America/New_York").expect("tz");
        let _scope = crate::datetime::SessionTimeZoneScope::enter(tz);
        assert_eq!(bind_sql(&tokyo).expect("rebind").sql, tokyo);
    }

    /// DST の欠落時刻は変換器が決定的に解決した UTC 値で保存される（元の micros を UTC として
    /// 流用しない）。
    #[test]
    fn session_local_timestamptz_literal_in_dst_gap_is_resolved_not_passed_through() {
        let tz = crate::datetime::SessionTimeZone::parse_setting("America/New_York").expect("tz");
        let _scope = crate::datetime::SessionTimeZoneScope::enter(tz);
        let sql = bind_sql("COALESCE(at_tz, TIMESTAMPTZ '2024-03-10 02:30:00')")
            .expect("bind")
            .sql;
        assert!(sql.contains("2024-03-10 07:30:00+00"), "{sql}");
    }

    /// 変換結果が範囲外になる `TIMESTAMPTZ` リテラルも拒否する。
    #[test]
    fn session_local_timestamptz_literal_out_of_range_fold_is_rejected() {
        let expr = Expr::TimestampTzLiteral {
            micros: i64::MAX,
            session_local: true,
        };
        assert!(fold_session_local_literals(&expr).is_err());
    }

    /// 式の最上位の `COLLATE` は束縛段でも `0A000`（構文段の最終ゲートの多層防御）。
    #[test]
    fn top_level_collate_is_rejected_at_binding() {
        let expr = crate::sql::allowlist::parse_index_expression_text("lang COLLATE \"C\"")
            .expect("parse");
        assert!(matches!(
            bind_index_expression(&expr, &bind_schema()),
            Err(SqlSurfaceError::FeatureNotSupported { .. })
        ));
    }
}

/// 式左辺の LIKE 述語（SQL-54、Issue #1781）が部分索引述語の決定性検査で
/// 式として検査されることの単体テスト。
#[cfg(test)]
mod like_lhs_expr_tests {
    use super::*;
    use crate::sql::udf_call::Expr;

    fn like_with_lhs(lhs: Expr) -> WherePredicate {
        WherePredicate::Prefix {
            lhs,
            pattern: "a%".to_string(),
            case_insensitive: false,
        }
    }

    #[test]
    fn bare_column_lhs_is_accepted_and_expr_lhs_is_rejected() {
        let col = like_with_lhs(Expr::Ident("name".to_string()));
        assert!(check_index_predicate_determinism(&[col]).is_ok());
        let nondet = like_with_lhs(Expr::Call {
            name: "random".to_string(),
            args: vec![],
        });
        assert!(check_index_predicate_determinism(&[nondet]).is_err());
        let det = like_with_lhs(Expr::Call {
            name: "lower".to_string(),
            args: vec![Expr::Ident("name".to_string())],
        });
        assert!(matches!(
            check_index_predicate_determinism(&[det]),
            Err(SqlSurfaceError::FeatureNotSupported { .. })
        ));
    }
}
