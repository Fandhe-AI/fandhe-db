//! `INSERT ... VALUES` の要素と列制約 `DEFAULT` で受理する定数式の畳み込み
//! （Issue #1543・SQL-45 のポインタ）。
//!
//! 責務境界: `sql::allowlist::Parser` が構文解析した列参照を含まない式 [`Expr`] を、
//! `sql::udf_call` の束縛器・評価器（第 2 の評価器は作らない）で評価し、既存の
//! [`InsertLiteral`] へ戻す。リテラルへ戻すため、束縛（`sql::parser`）・content_hash・
//! `ColumnDefault`・カタログ形式・wire-server は変更不要。呼び出し元は
//! `allowlist` の VALUES 行解析と列制約 `DEFAULT` 解析。
//!
//! fail-closed 方針: 事前走査は純粋に構文的でカタログを参照しない（DDL 権限ゲートより
//! 前に走っても存在オラクルにならない）。列参照は実在の有無によらず一律 `42703`、
//! 揮発性の値関数・組み込み以外の関数は `42601`、0 除算は `22012`、非有限値・f64 で
//! 正確に表現できない大きさは `22003` で拒否する。

use crate::sql::allowlist::{InsertLiteral, SqlSurfaceError};
use crate::sql::udf_call::{
    bind_const_expr, eval, first_non_builtin_call, BinOp, BoundExpr, Expr, ExprType, ExprValue,
    UdfRegistry, MAX_EXPR_NODES,
};

/// 仮値評価で起きたエラーの遅延方針（Issue #1546）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeferMode {
    /// 遅延しない（通常の parse。simple-query の畳み込みもこちら）。
    Off,
    /// `$n` を含む要素の値依存エラー（`22012`・`22003`）だけを Bind まで遅延する。
    Values,
    /// 構文確認専用。構文エラー（`42601`）以外はすべて握りつぶして最後まで走査する。
    Structural,
}

#[derive(Clone)]
struct DeferState {
    mode: DeferMode,
    /// 仮値置換後のトークン列で、元が `$n` だった位置（置換は 1 対 1 のため添字が一致する）。
    param_positions: Vec<usize>,
}

thread_local! {
    /// Parse 時の構造検証（パラメータ仮値での parse）中だけ有効になる遅延状態。
    /// 復元は [`DeferGuard`] の Drop が担い、panic 時も確実に元へ戻る（fail-closed）。
    static DEFER_STATE: std::cell::RefCell<DeferState> = const {
        std::cell::RefCell::new(DeferState { mode: DeferMode::Off, param_positions: Vec::new() })
    };
}

/// [`DEFER_STATE`] を元の状態へ戻す RAII ガード（panic 時も Drop で復元する）。
pub(crate) struct DeferGuard {
    prev: Option<DeferState>,
}

impl Drop for DeferGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.prev.take() {
            // thread-local 破棄後の Drop 等で失敗しても何もしない（`try_with`）。
            let _ = DEFER_STATE.try_with(|c| {
                if let Ok(mut st) = c.try_borrow_mut() {
                    *st = prev;
                }
            });
        }
    }
}

fn enter(mode: DeferMode, param_positions: Vec<usize>) -> DeferGuard {
    let prev = DEFER_STATE.with(|c| {
        c.replace(DeferState {
            mode,
            param_positions,
        })
    });
    DeferGuard { prev: Some(prev) }
}

/// 仮値評価による `$n` 由来の値依存エラー（`22012`・`22003`）を Bind まで遅延する区間を
/// 開始する。`engine` の Parse 経路（`parse_sql_prepared`。`$n` を含む文の構造検証）専用。
/// `param_positions` は仮値置換前のトークン列で `$n` だった位置。戻り値のガードが生きている
/// 間だけ有効で、Drop（panic 含む）で必ず元へ戻る。
pub(crate) fn defer_param_value_errors(param_positions: Vec<usize>) -> DeferGuard {
    enter(DeferMode::Values, param_positions)
}

/// 構文確認専用の区間を開始する。構文エラー（`42601`）以外の畳み込みエラーを無視して
/// 文全体を走査する（`42P18` より先に構文エラーを返す契約のため）。
pub(crate) fn defer_all_non_syntax_errors() -> DeferGuard {
    enter(DeferMode::Structural, Vec::new())
}

/// [`fold_const_literal`] の Parse 時構造検証版。`tokens` 内の位置範囲 `span` が要素の
/// トークン範囲。遅延モードでは、`Values` なら `span` が `$n` を含むときの値依存エラー、
/// `Structural` なら構文エラー以外のすべてを、プレースホルダ数値で置き換える。
pub(crate) fn fold_const_literal_deferrable(
    expr: &Expr,
    span: std::ops::Range<usize>,
) -> Result<InsertLiteral, SqlSurfaceError> {
    match fold_const_literal(expr) {
        Err(e) => {
            let defer = DEFER_STATE.with(|c| {
                let st = c.borrow();
                match st.mode {
                    DeferMode::Off => false,
                    DeferMode::Values => {
                        matches!(
                            e,
                            SqlSurfaceError::DivisionByZero { .. }
                                | SqlSurfaceError::NumericOutOfRange { .. }
                        ) && st.param_positions.iter().any(|p| span.contains(p))
                    }
                    DeferMode::Structural => e.wire_code() != "42601",
                }
            });
            if defer {
                Ok(InsertLiteral::Number("0".to_string()))
            } else {
                Err(e)
            }
        }
        ok => ok,
    }
}

/// f64 が整数を正確に表現できる上限（2^53）。
const MAX_EXACT_F64: f64 = 9_007_199_254_740_992.0;

/// 揮発性・セッション依存の SQL 値関数名（定数畳み込み対象外。`42601`）。
const VALUE_FUNCTION_NAMES: &[&str] = &[
    "CURRENT_TIMESTAMP",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "LOCALTIMESTAMP",
    "LOCALTIME",
    "CURRENT_USER",
    "SESSION_USER",
    "USER",
    "CURRENT_ROLE",
    "CURRENT_SCHEMA",
    "CURRENT_CATALOG",
    "DEFAULT",
];

/// 構文的な事前走査。`Expr` の全 variant を網羅する（`_ =>` を置かない）。
fn prescan(expr: &Expr) -> Result<(), SqlSurfaceError> {
    match expr {
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Null
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_)
        | Expr::TimeLiteral(_) => Ok(()),
        Expr::Ident(name) => {
            if VALUE_FUNCTION_NAMES
                .iter()
                .any(|v| v.eq_ignore_ascii_case(name))
            {
                Err(SqlSurfaceError::unsupported(
                    "volatile or session-dependent value is not allowed in a constant expression",
                ))
            } else if name.eq_ignore_ascii_case("true") || name.eq_ignore_ascii_case("false") {
                Err(SqlSurfaceError::FeatureNotSupported {
                    detail: "boolean keyword inside an expression is not supported".to_string(),
                })
            } else {
                Err(SqlSurfaceError::UndefinedColumn { name: name.clone() })
            }
        }
        // SQL-37（Issue #1586・#1587）: 現在時刻関数は、INSERT VALUES 内の式を台帳 v3（#2188）で
        // 扱えるようになるまで定数式で受理しない（`42601`）。列 `DEFAULT now()` 系（#1587）は
        // この定数式経路ではなく `sql::allowlist` の時刻 DEFAULT 専用経路が受理する。
        Expr::ValueFunction { .. } => Err(SqlSurfaceError::unsupported(
            "volatile or session-dependent value is not allowed in a constant expression",
        )),
        Expr::Call { name, .. } if crate::sql::udf_call::is_current_time_function_name(name) => {
            Err(SqlSurfaceError::unsupported(
                "volatile or session-dependent value is not allowed in a constant expression",
            ))
        }
        Expr::Call { args, .. } => args.iter().try_for_each(prescan),
        Expr::Binary { lhs, rhs, .. }
        | Expr::NullIf(lhs, rhs)
        | Expr::JsonAccess { lhs, rhs, .. } => {
            prescan(lhs)?;
            prescan(rhs)
        }
        Expr::Case { whens, else_result } => {
            for (c, r) in whens {
                prescan(c)?;
                prescan(r)?;
            }
            match else_result {
                Some(e) => prescan(e),
                None => Ok(()),
            }
        }
        Expr::Coalesce(args) => args.iter().try_for_each(prescan),
    }
}

/// 算術・関数ノードの中間結果が f64 で整数を正確に表現できる範囲（|v| < 2^53）を
/// 超えていないかを検査する。`9007199254740992 + 1 - 9007199254740992` のように
/// 中間で丸められた値が最終結果の範囲検査を通過して誤った整数になるのを `22003` で
/// 防ぐ（fail-closed）。サブ式の評価エラーは（実行されない CASE 分岐等で偽陽性に
/// ならないよう）ここでは無視し、最終評価側に報告させる。
fn check_intermediate_precision(expr: &Expr) -> Result<(), SqlSurfaceError> {
    let out_of_range =
        || SqlSurfaceError::numeric_out_of_range("constant expression loses integer precision");
    match expr {
        Expr::Number(n) => {
            // 整数リテラルは f64 化で丸められる前に桁で判定する。
            if n.bytes().all(|b| b.is_ascii_digit())
                && n.parse::<u128>()
                    .map_or(true, |v| v > MAX_EXACT_F64 as u128)
            {
                return Err(out_of_range());
            }
            Ok(())
        }
        Expr::String(_)
        | Expr::Null
        | Expr::Ident(_)
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_)
        | Expr::TimeLiteral(_)
        | Expr::ValueFunction { .. } => Ok(()),
        Expr::Call { args, .. } => {
            args.iter().try_for_each(check_intermediate_precision)?;
            check_node_value(expr, true)
        }
        Expr::Binary { lhs, rhs, .. } => {
            check_intermediate_precision(lhs)?;
            check_intermediate_precision(rhs)?;
            check_node_value(expr, true)
        }
        // jsonb パス演算子は列参照を要し、定数式では `prescan` が列参照として拒否する。
        Expr::JsonAccess { .. } => Ok(()),
        Expr::NullIf(lhs, rhs) => {
            check_intermediate_precision(lhs)?;
            check_intermediate_precision(rhs)
        }
        // COALESCE は短絡評価（`udf_call::eval` と同じ）に沿い、評価される引数
        // （最初の非 NULL まで）だけを検査する。未評価引数の巨大値で偽陽性にしない。
        Expr::Coalesce(args) => {
            for a in args {
                if matches!(a, Expr::Null) {
                    continue;
                }
                check_intermediate_precision(a)?;
                if !matches!(eval_node(a), Some(ExprValue::Null)) {
                    break;
                }
            }
            Ok(())
        }
        // CASE も短絡評価に沿い、条件を順に評価して選ばれる分岐だけを検査する。
        // 条件が評価不能なら以降は最終評価側に報告させる。
        Expr::Case { whens, else_result } => {
            for (c, r) in whens {
                check_intermediate_precision(c)?;
                match eval_node(c) {
                    Some(ExprValue::Bool(true)) => return check_intermediate_precision(r),
                    Some(ExprValue::Bool(false) | ExprValue::Null) => {}
                    _ => return Ok(()),
                }
            }
            match else_result {
                Some(e) => check_intermediate_precision(e),
                None => Ok(()),
            }
        }
    }
}

/// 1 ノードを束縛・評価する（束縛・評価不能は `None`。短絡判定専用）。
fn eval_node(expr: &Expr) -> Option<ExprValue<'static>> {
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    // 裸の NULL を含む式は型付き NULL へ畳んでから束縛し、短絡判定に使えるようにする。
    let folded = propagate_null(expr).ok()?;
    let (bound, _ty, _) = bind_const_expr(&folded, &registry, &mut budget).ok()?;
    eval(&bound, 0, &[]).ok()
}

/// 1 ノードを単独評価し、スカラ結果が 2^53 以上なら `22003`（評価不能は無視）。
fn check_node_value(expr: &Expr, enabled: bool) -> Result<(), SqlSurfaceError> {
    if !enabled {
        return Ok(());
    }
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    let Ok((bound, _ty, _)) = bind_const_expr(expr, &registry, &mut budget) else {
        return Ok(());
    };
    if let Ok(ExprValue::Scalar(v)) = eval(&bound, 0, &[]) {
        if v.is_finite() && v.abs() >= MAX_EXACT_F64 {
            return Err(SqlSurfaceError::numeric_out_of_range(
                "constant expression loses integer precision",
            ));
        }
    }
    Ok(())
}

/// 型付き `NULL` の型 `ty` を表す束縛可能な式（評価結果は常に `NULL`）。`NULL` へ畳んだ
/// 演算結果の型情報を保持し、`COALESCE`／`CASE` の兄弟枝との型突き合わせ（`42804`）を
/// 保つ。Bool は `NULLIF` が受理しないため分岐なしの `CASE` で表す。
fn typed_null(ty: ExprType) -> Result<Expr, SqlSurfaceError> {
    let twin = |e: Expr| Expr::NullIf(Box::new(e.clone()), Box::new(e));
    Ok(match ty {
        ExprType::Scalar => twin(Expr::Number("0".to_string())),
        ExprType::Text => twin(Expr::String(String::new())),
        ExprType::Date => twin(Expr::DateLiteral(0)),
        ExprType::Timestamp => twin(Expr::TimestampLiteral(0)),
        ExprType::Time => twin(Expr::TimeLiteral(0)),
        ExprType::Json => {
            return Err(SqlSurfaceError::unsupported(
                "a jsonb value is not allowed in a constant expression",
            ))
        }
        ExprType::Bool => Expr::Case {
            whens: vec![(
                Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Number("1".to_string())),
                    rhs: Box::new(Expr::Number("0".to_string())),
                },
                Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Number("1".to_string())),
                    rhs: Box::new(Expr::Number("1".to_string())),
                },
            )],
            else_result: None,
        },
        ExprType::Vector => {
            return Err(SqlSurfaceError::datatype_mismatch(
                "a vector-typed NULL is not allowed in a constant expression",
            ))
        }
    })
}

/// `e` が `NULL` 定数なら `Some(型)`（裸の `NULL` は `Some(None)`、[`typed_null`] の
/// 生成物は `Some(Some(型))`）。それ以外は `None`。
fn null_kind(e: &Expr) -> Option<Option<ExprType>> {
    match e {
        Expr::Null => Some(None),
        Expr::NullIf(a, b) if a == b => match &**a {
            Expr::Number(n) if n == "0" => Some(Some(ExprType::Scalar)),
            Expr::String(s) if s.is_empty() => Some(Some(ExprType::Text)),
            Expr::DateLiteral(0) => Some(Some(ExprType::Date)),
            Expr::TimestampLiteral(0) => Some(Some(ExprType::Timestamp)),
            Expr::TimeLiteral(0) => Some(Some(ExprType::Time)),
            _ => None,
        },
        Expr::Case { whens, else_result } if else_result.is_none() && whens.len() == 1 => {
            if *e == typed_null(ExprType::Bool).ok()? {
                Some(Some(ExprType::Bool))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `NULL` オペランドの型候補（`NULL` 側の位置に置いて束縛を試す順序。算術で曖昧な
/// `NULL + 1` は PostgreSQL と同様に Scalar を優先する）。
const NULL_OPERAND_CANDIDATES: [ExprType; 5] = [
    ExprType::Scalar,
    ExprType::Text,
    ExprType::Date,
    ExprType::Timestamp,
    ExprType::Time,
];

/// 関数呼び出しの `NULL` 引数に対する型候補の組み合わせ試行回数の上限（DoS 防止）。
const MAX_NULL_ARG_PROBES: usize = 256;

/// 1 オペランドが取りうる置換候補。非 `NULL` は式そのもの、型既知の `NULL` は
/// 型付き `NULL`、裸の `NULL` は型候補ごとの型付き `NULL`。
fn operand_options(e: &Expr, k: Option<Option<ExprType>>) -> Vec<Expr> {
    match k {
        None => vec![e.clone()],
        Some(Some(t)) => typed_null(t).into_iter().collect(),
        Some(None) => NULL_OPERAND_CANDIDATES
            .iter()
            .filter_map(|t| typed_null(*t).ok())
            .collect(),
    }
}

/// `NULL` を含む二項演算を、演算子の型規則に従って束縛可能な式へ置き換える。
/// `NULL` 側のオペランド位置を保ったまま型付き `NULL` 候補を順に束縛し、最初に束縛できた
/// 組を採る。両辺が `NULL` なら結果型の型付き `NULL` へ畳む。非 `NULL` 側がある場合は
/// 式を残し、最終評価で非 `NULL` 側の評価エラー（`1/0` の `22012` 等）を `NULL` より
/// 優先して伝播させる（PostgreSQL と同じ。選ばれない CASE／COALESCE 枝では評価されない
/// ため短絡も保たれる）。1 つも束縛できなければ最初のエラーを返す。
fn fold_null_binary(
    op: BinOp,
    l: &Expr,
    r: &Expr,
    ln: Option<Option<ExprType>>,
    rn: Option<Option<ExprType>>,
) -> Result<Expr, SqlSurfaceError> {
    let mut first_err: Option<SqlSurfaceError> = None;
    for lhs in operand_options(l, ln) {
        for rhs in operand_options(r, rn) {
            let probe = Expr::Binary {
                op,
                lhs: Box::new(lhs.clone()),
                rhs: Box::new(rhs),
            };
            let registry = UdfRegistry::default();
            let mut budget = MAX_EXPR_NODES;
            match bind_const_expr(&probe, &registry, &mut budget) {
                Ok((_, ty, _)) => {
                    return if ln.is_some() && rn.is_some() {
                        typed_null(ty)
                    } else {
                        Ok(probe)
                    };
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
    }
    Err(first_err.unwrap_or_else(|| {
        SqlSurfaceError::datatype_mismatch("NULL operand type cannot be resolved")
    }))
}

/// 裸の `NULL` 引数を持つ関数呼び出しへ型付き `NULL` を割り当てる。まず元の呼び出しが
/// そのまま束縛できれば（`CONCAT` 等）変更しない。束縛できなければ裸の `NULL` 位置ごとに
/// 型候補を順に試し（組み合わせ数は [`MAX_NULL_ARG_PROBES`] で打ち切る）、最初に束縛
/// できた組を採る。他の引数は式のまま残すため、その評価エラーは最終評価で伝播する。
fn fold_null_call(name: &str, args: Vec<Expr>) -> Result<Expr, SqlSurfaceError> {
    let try_bind = |args: &[Expr]| {
        let probe = Expr::Call {
            name: name.to_string(),
            args: args.to_vec(),
        };
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        bind_const_expr(&probe, &registry, &mut budget).map(|_| probe)
    };
    if !args.iter().any(|a| matches!(a, Expr::Null)) {
        return Ok(Expr::Call {
            name: name.to_string(),
            args,
        });
    }
    let first_err = match try_bind(&args) {
        Ok(p) => return Ok(p),
        Err(e) => e,
    };
    let options: Vec<Vec<Expr>> = args
        .iter()
        .map(|a| operand_options(a, null_kind(a)))
        .collect();
    let mut idx = vec![0usize; args.len()];
    for _ in 0..MAX_NULL_ARG_PROBES {
        let cand: Option<Vec<Expr>> = idx
            .iter()
            .zip(&options)
            .map(|(i, o)| o.get(*i).cloned())
            .collect();
        if let Some(Ok(p)) = cand.map(|c| try_bind(&c)) {
            return Ok(p);
        }
        // 位取りの繰り上げで次の組み合わせへ進む（全て回りきったら終了）。
        let mut pos = 0;
        loop {
            let Some(slot) = idx.get_mut(pos) else {
                return Err(first_err);
            };
            *slot += 1;
            if options.get(pos).is_some_and(|o| *slot < o.len()) {
                break;
            }
            *slot = 0;
            pos += 1;
        }
    }
    Err(first_err)
}

/// 算術・比較の直接オペランドに `NULL` を持つ二項演算を型付き `NULL` へ畳む
/// （3 値論理の NULL 伝播。束縛器は型を決められない裸の `NULL` を `0A000` で
/// 拒否するため、束縛前に定数として確定させる）。CASE／COALESCE／NULLIF 内の
/// `NULL` は束縛器が扱うのでそのまま残す。
fn propagate_null(expr: &Expr) -> Result<Expr, SqlSurfaceError> {
    Ok(match expr {
        Expr::Binary { op, lhs, rhs } => {
            let l = propagate_null(lhs)?;
            let r = propagate_null(rhs)?;
            let (ln, rn) = (null_kind(&l), null_kind(&r));
            if ln.is_some() || rn.is_some() {
                fold_null_binary(*op, &l, &r, ln, rn)?
            } else {
                Expr::Binary {
                    op: *op,
                    lhs: Box::new(l),
                    rhs: Box::new(r),
                }
            }
        }
        Expr::Call { name, args } => fold_null_call(
            name,
            args.iter().map(propagate_null).collect::<Result<_, _>>()?,
        )?,
        Expr::Coalesce(args) => {
            Expr::Coalesce(args.iter().map(propagate_null).collect::<Result<_, _>>()?)
        }
        Expr::NullIf(l, r) => {
            Expr::NullIf(Box::new(propagate_null(l)?), Box::new(propagate_null(r)?))
        }
        Expr::Case { whens, else_result } => Expr::Case {
            whens: whens
                .iter()
                .map(|(c, r)| Ok((propagate_null(c)?, propagate_null(r)?)))
                .collect::<Result<_, SqlSurfaceError>>()?,
            else_result: match else_result {
                Some(e) => Some(Box::new(propagate_null(e)?)),
                None => None,
            },
        },
        other => other.clone(),
    })
}

/// 列参照を含まない式を評価して [`InsertLiteral`] へ畳み込む。
pub(crate) fn fold_const_literal(expr: &Expr) -> Result<InsertLiteral, SqlSurfaceError> {
    prescan(expr)?;
    if first_non_builtin_call(expr).is_some() {
        return Err(SqlSurfaceError::unsupported(
            "only built-in functions are allowed in a constant expression",
        ));
    }
    // 最終評価は NULL 伝播（型付き NULL への畳み込み）後の式で行う。
    // 精度検査は NULL 畳み込み前の元の式に対し短絡評価で選ばれた枝だけで行う
    // （畳み込みは型検証専用。未評価枝の精度欠落を 22003 にしない）。
    check_intermediate_precision(expr)?;
    let propagated = propagate_null(expr)?;
    if matches!(propagated, Expr::Null) {
        return Ok(InsertLiteral::Null);
    }
    let expr = &propagated;
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    let (bound, _ty, _) = bind_const_expr(expr, &registry, &mut budget)?;
    if crate::sql::udf_call::references_embedding(&bound) || matches!(bound, BoundExpr::IdRef) {
        return Err(SqlSurfaceError::Internal {
            detail: "constant expression resolved to a row reference".to_string(),
        });
    }
    match eval(&bound, 0, &[])? {
        ExprValue::Scalar(v) => {
            if !v.is_finite() || v.abs() > MAX_EXACT_F64 {
                return Err(SqlSurfaceError::numeric_out_of_range(
                    "constant expression result is out of range",
                ));
            }
            if v.fract() == 0.0 {
                // -0.0 は "0" へ正規化する（i64 変換は |v| <= 2^53 を確認済み）。
                Ok(InsertLiteral::Number(format!("{}", v as i64)))
            } else {
                Ok(InsertLiteral::Number(format!("{v}")))
            }
        }
        ExprValue::Text(s) => {
            if s.len() > crate::row_codec::MAX_TEXT_FIELD_LEN as usize {
                return Err(SqlSurfaceError::payload_too_large(
                    "constant expression result exceeds the maximum TEXT field length",
                ));
            }
            Ok(InsertLiteral::String(s.into_owned()))
        }
        ExprValue::Bool(b) => Ok(InsertLiteral::Bool(b)),
        ExprValue::Null => Ok(InsertLiteral::Null),
        ExprValue::Date(d) => Ok(InsertLiteral::String(crate::datetime::format_date(d))),
        ExprValue::Timestamp(t) => Ok(InsertLiteral::String(crate::datetime::format_timestamp(t))),
        ExprValue::Time(t) => Ok(InsertLiteral::String(crate::datetime::format_time(t))),
        ExprValue::Vector(_) => Err(SqlSurfaceError::Internal {
            detail: "constant expression produced a vector".to_string(),
        }),
        ExprValue::Json(_) => Err(SqlSurfaceError::unsupported(
            "a jsonb value is not allowed in a constant expression",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(s: &str) -> Expr {
        Expr::Number(s.to_string())
    }
    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary {
            op,
            lhs: Box::new(l),
            rhs: Box::new(r),
        }
    }
    fn call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            name: name.to_string(),
            args,
        }
    }

    #[test]
    fn folds_arithmetic_and_functions() {
        assert_eq!(
            fold_const_literal(&bin(BinOp::Add, num("1"), num("1"))).unwrap(),
            InsertLiteral::Number("2".into())
        );
        assert_eq!(
            fold_const_literal(&call("upper", vec![Expr::String("a".into())])).unwrap(),
            InsertLiteral::String("A".into())
        );
        // 整数同士の除算は整数除算（main の型付き算術。PostgreSQL と同じ）、小数を含めば小数。
        assert_eq!(
            fold_const_literal(&bin(BinOp::Div, num("1"), num("2"))).unwrap(),
            InsertLiteral::Number("0".into())
        );
        assert_eq!(
            fold_const_literal(&bin(BinOp::Div, num("1.0"), num("2"))).unwrap(),
            InsertLiteral::Number("0.5".into())
        );
        assert_eq!(
            fold_const_literal(&Expr::Coalesce(vec![Expr::Null, num("7")])).unwrap(),
            InsertLiteral::Number("7".into())
        );
        assert_eq!(
            fold_const_literal(&Expr::NullIf(Box::new(num("1")), Box::new(num("1")))).unwrap(),
            InsertLiteral::Null
        );
    }

    #[test]
    fn date_roundtrip() {
        let d = crate::datetime::parse_date("2024-01-02").unwrap();
        let InsertLiteral::String(s) = fold_const_literal(&Expr::DateLiteral(d)).unwrap() else {
            panic!("expected string");
        };
        assert_eq!(crate::datetime::parse_date(&s).unwrap(), d);
        let t = crate::datetime::parse_timestamp("2024-01-02 03:04:05").unwrap();
        let InsertLiteral::String(s) = fold_const_literal(&Expr::TimestampLiteral(t)).unwrap()
        else {
            panic!("expected string");
        };
        assert_eq!(crate::datetime::parse_timestamp(&s).unwrap(), t);
    }

    #[test]
    fn rejects_column_refs_with_42703() {
        for e in [
            Expr::Ident("n".into()),
            Expr::Ident("id".into()),
            call("upper", vec![Expr::Ident("n".into())]),
        ] {
            assert!(matches!(
                fold_const_literal(&e),
                Err(SqlSurfaceError::UndefinedColumn { .. })
            ));
        }
    }

    #[test]
    fn rejects_volatile_and_udf_with_42601() {
        for e in [
            Expr::Ident("CURRENT_TIMESTAMP".into()),
            call("now", vec![]),
            call("my_udf", vec![num("1")]),
        ] {
            assert!(
                matches!(
                    fold_const_literal(&e),
                    Err(SqlSurfaceError::UnsupportedSyntax { .. })
                ),
                "{e:?}"
            );
        }
        assert!(matches!(
            fold_const_literal(&Expr::Ident("true".into())),
            Err(SqlSurfaceError::FeatureNotSupported { .. })
        ));
    }

    #[test]
    fn rejects_div_zero_and_range() {
        assert!(matches!(
            fold_const_literal(&bin(BinOp::Div, num("1"), num("0"))),
            Err(SqlSurfaceError::DivisionByZero { .. })
        ));
        assert!(matches!(
            fold_const_literal(&bin(BinOp::Mul, num("9007199254740993"), num("2"))),
            Err(SqlSurfaceError::NumericOutOfRange { .. })
        ));
        // 中間結果の丸め（2^53 + 1 → 2^53）を 22003 で検出する。
        let big = bin(BinOp::Add, num("9007199254740992"), num("1"));
        assert!(matches!(
            fold_const_literal(&bin(BinOp::Sub, big, num("9007199254740992"))),
            Err(SqlSurfaceError::NumericOutOfRange { .. })
        ));
        assert!(matches!(
            fold_const_literal(&bin(BinOp::Add, num("9007199254740993"), num("0"))),
            Err(SqlSurfaceError::NumericOutOfRange { .. })
        ));
        assert!(fold_const_literal(&bin(BinOp::Add, Expr::String("a".into()), num("1"))).is_err());
    }

    #[test]
    fn unselected_branches_do_not_trigger_precision_error() {
        let p54 = || call("power", vec![num("2"), num("54")]);
        let case = Expr::Case {
            whens: vec![(bin(BinOp::Eq, num("1"), num("1")), num("1"))],
            else_result: Some(Box::new(p54())),
        };
        assert_eq!(
            fold_const_literal(&case).unwrap(),
            InsertLiteral::Number("1".into())
        );
        assert_eq!(
            fold_const_literal(&Expr::Coalesce(vec![num("1"), p54()])).unwrap(),
            InsertLiteral::Number("1".into())
        );
        // 選ばれる分岐は従来どおり検査する。
        assert!(matches!(
            fold_const_literal(&Expr::Coalesce(vec![Expr::Null, p54()])),
            Err(SqlSurfaceError::NumericOutOfRange { .. })
        ));
    }

    #[test]
    fn null_operand_still_validates_other_side() {
        let bad = call("upper", vec![num("1")]);
        assert!(fold_const_literal(&bin(BinOp::Add, Expr::Null, bad)).is_err());
        assert!(
            fold_const_literal(&bin(BinOp::Add, Expr::String("a".into()), Expr::Null)).is_err()
        );
        assert_eq!(
            fold_const_literal(&bin(BinOp::Add, Expr::Null, num("1"))).unwrap(),
            InsertLiteral::Null
        );
    }

    #[test]
    fn coalesce_with_null_first_arg_still_checks_later_precision() {
        let big = bin(
            BinOp::Sub,
            bin(BinOp::Add, num("9007199254740992"), num("1")),
            num("9007199254740992"),
        );
        let e = Expr::Coalesce(vec![bin(BinOp::Add, Expr::Null, num("1")), big]);
        assert!(matches!(
            fold_const_literal(&e),
            Err(SqlSurfaceError::NumericOutOfRange { .. })
        ));
    }

    #[test]
    fn folded_null_keeps_operand_type() {
        let nul_plus_1 = || bin(BinOp::Add, Expr::Null, num("1"));
        assert!(matches!(
            fold_const_literal(&Expr::Coalesce(vec![
                nul_plus_1(),
                Expr::String("x".into())
            ])),
            Err(SqlSurfaceError::DatatypeMismatch { .. })
        ));
        let case = Expr::Case {
            whens: vec![(bin(BinOp::Eq, num("1"), num("1")), nul_plus_1())],
            else_result: Some(Box::new(Expr::String("x".into()))),
        };
        assert!(matches!(
            fold_const_literal(&case),
            Err(SqlSurfaceError::DatatypeMismatch { .. })
        ));
        assert_eq!(
            fold_const_literal(&Expr::Coalesce(vec![nul_plus_1(), Expr::Null])).unwrap(),
            InsertLiteral::Null
        );
        assert_eq!(
            fold_const_literal(&Expr::Coalesce(vec![nul_plus_1(), num("5")])).unwrap(),
            InsertLiteral::Number("5".into())
        );
    }

    #[test]
    fn null_probe_respects_operand_position() {
        let d = || Expr::DateLiteral(0);
        // DATE + NULL は DATE（日数 NULL）として受理、NULL - DATE は DATE - DATE として受理される。
        assert_eq!(
            fold_const_literal(&bin(BinOp::Add, d(), Expr::Null)).unwrap(),
            InsertLiteral::Null
        );
        assert_eq!(
            fold_const_literal(&bin(BinOp::Sub, Expr::Null, d())).unwrap(),
            InsertLiteral::Null
        );
        // NULL - DATE の結果は Scalar 型（日数差）。
        assert!(matches!(
            fold_const_literal(&Expr::Coalesce(vec![
                bin(BinOp::Sub, Expr::Null, d()),
                Expr::String("x".into())
            ])),
            Err(SqlSurfaceError::DatatypeMismatch { .. })
        ));
        // DATE + NULL の結果は DATE 型。
        assert!(matches!(
            fold_const_literal(&Expr::Coalesce(vec![
                bin(BinOp::Add, d(), Expr::Null),
                num("1")
            ])),
            Err(SqlSurfaceError::DatatypeMismatch { .. })
        ));
    }

    #[test]
    fn unevaluated_branch_null_precision_is_not_checked() {
        let big = || {
            bin(
                BinOp::Add,
                Expr::Null,
                call("power", vec![num("2"), num("54")]),
            )
        };
        // COALESCE は先頭引数で確定し、後続の NULL 演算内の精度欠落を検査しない。
        assert_eq!(
            fold_const_literal(&Expr::Coalesce(vec![num("1"), big()])).unwrap(),
            InsertLiteral::Number("1".into())
        );
        // CASE も選ばれない ELSE 枝は検査しない。
        assert_eq!(
            fold_const_literal(&Expr::Case {
                whens: vec![(bin(BinOp::Eq, num("1"), num("1")), num("1"))],
                else_result: Some(Box::new(big())),
            })
            .unwrap(),
            InsertLiteral::Number("1".into())
        );
        // 評価される枝では従来どおり 22003。
        assert!(fold_const_literal(&Expr::Coalesce(vec![Expr::Null, big()])).is_err());
    }

    #[test]
    fn null_operand_propagates_other_side_eval_error() {
        let div0 = || bin(BinOp::Div, num("1"), num("0"));
        for e in [
            bin(BinOp::Add, Expr::Null, div0()),
            bin(BinOp::Mul, div0(), Expr::Null),
            Expr::Coalesce(vec![bin(BinOp::Add, Expr::Null, div0()), num("5")]),
        ] {
            assert!(
                matches!(
                    fold_const_literal(&e),
                    Err(SqlSurfaceError::DivisionByZero { .. })
                ),
                "{e:?}"
            );
        }
    }

    #[test]
    fn null_function_args_fold_to_null() {
        assert_eq!(
            fold_const_literal(&call("upper", vec![Expr::Null])).unwrap(),
            InsertLiteral::Null
        );
        assert_eq!(
            fold_const_literal(&call("abs", vec![Expr::Null])).unwrap(),
            InsertLiteral::Null
        );
        assert!(matches!(
            fold_const_literal(&call("abs", vec![bin(BinOp::Div, num("1"), num("0"))])),
            Err(SqlSurfaceError::DivisionByZero { .. })
        ));
    }
}
