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
//!
//! Issue #2881: 2^53 以上の値は、精度を保って評価できる形だけ受理する。整数どうしの算術・比較
//! （`udf_call` の整数モード。bigint 全域を i128 で丸めなく評価し、範囲外は `22003`）と、
//! PG の float8／float4 型の式（内部専用の型付き値を含む `+ - * /`。有限性だけを検査する）が対象。
//! 型なしの小数リテラルなど f64 に丸めうる形、整数の運び手が関数・`NULLIF`・配列・浮動小数の
//! 算術へ流れる形は従来どおり `22003` で拒否する（黙った丸めを返さない）。

use crate::sql::allowlist::{InsertLiteral, SqlSurfaceError};
use crate::sql::udf_call::{
    bind_const_expr_array_top_mode, bind_const_expr_mode, contains_typed_numeric, eval,
    first_non_builtin_call_with, BinOp, BoundExpr, BuiltinFn, Expr, ExprType, ExprValue,
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

/// f64 の経路（型なしの小数リテラル・関数の結果等）が `22003` にする大きさか（|v| >= 2^53）。
/// `plpgsql::subst`／`sql::params` が「単独の要素を裸の数値で渡す」範囲を判定する際も同じ定義を
/// 使う（境界の二重定義による食い違いを防ぐ）。Issue #2881 以降、整数どうしの算術・比較と
/// float 型の式の中の同範囲の値は畳み込みが正確に扱う。非有限値は対象外。
pub(crate) fn float_beyond_fold_range(v: f64) -> bool {
    v.is_finite() && v.abs() >= MAX_EXACT_F64
}

/// 整数の絶対値版の [`float_beyond_fold_range`]（|v| >= 2^53）。
pub(crate) fn int_magnitude_beyond_fold_range(magnitude: u64) -> bool {
    u128::from(magnitude) >= MAX_EXACT_F64 as u128
}

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
        // 定数式文脈（INSERT VALUES・DEFAULT 等）の `COLLATE` は従来どおり `42601`
        // （TABLE-28・Issue #1609 の対象外。比較・格納の照合対応は #1610）。
        Expr::Collate { .. } => Err(SqlSurfaceError::unsupported(
            "COLLATE is not supported in a constant expression",
        )),
        // SQL/JSON 問い合わせ関数（TABLE-25・Issue #2187）は定数式（INSERT VALUES・DEFAULT 等）
        // では受理しない（`0A000`。FROM なし SELECT は `const_select` の別経路が受理する）。
        Expr::SqlJsonQuery(_) => Err(SqlSurfaceError::FeatureNotSupported {
            detail: "SQL/JSON query functions are not supported in a constant expression"
                .to_string(),
        }),
        // `CAST`（SQL-47・Issue #1590）も定数式（INSERT VALUES・DEFAULT 等）では未対応（`0A000`）。
        // 値の変換は #1591／#1592 の担当で、評価できない式を定数として黙って通さない。
        Expr::Cast { .. } => Err(SqlSurfaceError::FeatureNotSupported {
            detail: "CAST is not supported in a constant expression".to_string(),
        }),
        // 配列コンストラクタ・添字（TABLE-35・Issue #1602）は子を再帰で検査する（列参照は
        // `Expr::Ident` の腕で `42703`）。評価結果の配列は `fold_const_literal` が配列テキストへ畳む。
        Expr::ArrayConstructor(_) | Expr::Subscript { .. } | Expr::Quantified { .. } => expr
            .array_expr_children()
            .into_iter()
            .flatten()
            .try_for_each(prescan),
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Null
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_)
        | Expr::IntervalLiteral(_)
        | Expr::TimeLiteral(_)
        | Expr::TimestampTzLiteral { .. } => Ok(()),
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
        // SQL-37（Issue #1586・#1587・#2188）: 現在時刻関数は定数式（畳み込み）では受理しない
        // （`42601`）。要素全体が単独の時刻関数である `VALUES` 要素と列 `DEFAULT` は、この定数式経路
        // ではなく `sql::allowlist` の時刻関数専用経路（`InsertLiteral::CurrentTime`・
        // `TimeDefault`）が受理する。合成式（`now() + …`）は後続 Issue。
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
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::RegexMatch {
            lhs, pattern: rhs, ..
        }
        | Expr::JsonAccess { lhs, rhs, .. }
        | Expr::VectorDistance { lhs, rhs, .. } => {
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
fn check_intermediate_precision(expr: &Expr, numeric: bool) -> Result<(), SqlSurfaceError> {
    let out_of_range =
        || SqlSurfaceError::numeric_out_of_range("constant expression loses integer precision");
    match expr {
        Expr::Number(n) => {
            // 整数リテラルは f64 化で丸められる前に桁で判定する。整数モードは葉の上限（2^63）まで
            // 受理する（運び手として丸めずに束縛され、値としての可否は利用側の検査が決める）。
            let limit = if numeric {
                MAX_EXACT_F64 as u128
            } else {
                EXACT_LEAF_MAX
            };
            // 2^63 の葉は `0 - 2^63`（i64::MIN の脱糖形）の右辺でだけ許す（下の Binary 腕）。
            // 単独・他の演算の位置で残る 2^63 は bigint の切り捨てを招くため `22003`。
            if n.bytes().all(|b| b.is_ascii_digit())
                && n.parse::<u128>().map_or(true, |v| v > limit)
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
        | Expr::IntervalLiteral(_)
        | Expr::TimeLiteral(_)
        | Expr::TimestampTzLiteral { .. }
        | Expr::ValueFunction { .. } => Ok(()),
        Expr::Collate { expr, .. } => check_intermediate_precision(expr, numeric),
        // 到達しない（`prescan` が拒否済み）。
        Expr::SqlJsonQuery(_) | Expr::Cast { .. } => Ok(()),
        // 配列の要素・添字は子ごとに精度を検査する（要素値の範囲は配列の構築側が検査する）。
        Expr::ArrayConstructor(_) | Expr::Subscript { .. } | Expr::Quantified { .. } => {
            expr.array_expr_children()
                .into_iter()
                .flatten()
                .try_for_each(|c| check_intermediate_precision(c, numeric))?;
            reject_carrier_children(expr.array_expr_children().into_iter().flatten(), numeric)
        }
        // Issue #2799: 型付き NUMERIC 値の raw 数値は 2^53 を超えてよい（Decimal で正確に運ぶ）ため、
        // f64 前提の桁検査を適用しない。
        Expr::Call { name, .. } if crate::sql::udf_call::is_typed_numeric_name(name) => Ok(()),
        // Issue #2881: 整数モードの内部専用の型付き値は、束縛段が型の範囲（整数）・有限性（浮動小数）を
        // 検査して丸めず束縛するため、f64 前提の 2^53 検査を適用しない。
        Expr::Call { name, .. } if !numeric && crate::sql::udf_call::is_typed_value_name(name) => {
            Ok(())
        }
        Expr::Call { args, .. } => {
            args.iter()
                .try_for_each(|c| check_intermediate_precision(c, numeric))?;
            reject_carrier_children(args.iter(), numeric)?;
            check_node_value(expr, true, numeric)
        }
        // Issue #2799: NUMERIC 算術（結果が Numeric に束縛される二項演算）は f64 を経由しないため、
        // 直下の数値リテラルの桁検査と f64 専用の結果検査を省き、非リテラルの子だけ再帰する。
        Expr::Binary { lhs, rhs, .. } if numeric && binds_to_numeric(expr) => [lhs, rhs]
            .into_iter()
            .filter(|c| !matches!(&***c, Expr::Number(_)))
            .try_for_each(|c| check_intermediate_precision(c, numeric)),
        Expr::Binary { op, lhs, rhs } => {
            check_intermediate_precision(lhs, numeric)?;
            let is_min_desugar = !numeric
                && matches!(op, BinOp::Sub)
                && matches!(&**lhs, Expr::Number(l) if l == "0")
                && matches!(&**rhs, Expr::Number(r) if r == MIN_LEAF_DIGITS);
            if !is_min_desugar {
                check_intermediate_precision(rhs, numeric)?;
            }
            if numeric {
                return check_node_value(expr, true, numeric);
            }
            // Issue #2881: 整数どうしの算術・比較は丸めなく評価されるため 2^53 の値検査を外す。
            if binds_to_exact_int(expr) {
                return Ok(());
            }
            // 整数の運び手は整数算術・整数比較以外（浮動小数との混合など）へは流せない。
            reject_carrier_children([&**lhs, &**rhs].into_iter(), numeric)?;
            // PG の float8／float4 型の式は f64 が正しい意味論のため、有限性だけを検査する。
            if pg_float_typed(expr) {
                check_untyped_decimal_leaves(expr)?;
                return check_node_finite(expr);
            }
            check_node_value(expr, true, numeric)
        }
        // jsonb パス演算子は列参照を要し、定数式では `prescan` が列参照として拒否する。
        Expr::JsonAccess { .. } => Ok(()),
        // 距離式（SQL-74・Issue #2035）は定数式として畳み込まない（束縛段が `0A000` で拒否する）。
        Expr::VectorDistance { .. } => Ok(()),
        Expr::NullIf(lhs, rhs)
        | Expr::IsDistinctFrom { lhs, rhs, .. }
        | Expr::RegexMatch {
            lhs, pattern: rhs, ..
        } => {
            check_intermediate_precision(lhs, numeric)?;
            check_intermediate_precision(rhs, numeric)?;
            reject_carrier_children([&**lhs, &**rhs].into_iter(), numeric)
        }
        // COALESCE は短絡評価（`udf_call::eval` と同じ）に沿い、評価される引数
        // （最初の非 NULL まで）だけを検査する。未評価引数の巨大値で偽陽性にしない。
        Expr::Coalesce(args) => {
            for a in args {
                if matches!(a, Expr::Null) {
                    continue;
                }
                check_intermediate_precision(a, numeric)?;
                if !matches!(eval_node(a, numeric), Some(ExprValue::Null)) {
                    break;
                }
            }
            Ok(())
        }
        // CASE も短絡評価に沿い、条件を順に評価して選ばれる分岐だけを検査する。
        // 条件が評価不能なら以降は最終評価側に報告させる。
        Expr::Case { whens, else_result } => {
            for (c, r) in whens {
                check_intermediate_precision(c, numeric)?;
                match eval_node(c, numeric) {
                    Some(ExprValue::Bool(true)) => return check_intermediate_precision(r, numeric),
                    Some(ExprValue::Bool(false) | ExprValue::Null) => {}
                    _ => return Ok(()),
                }
            }
            match else_result {
                Some(e) => check_intermediate_precision(e, numeric),
                None => Ok(()),
            }
        }
    }
}

/// 整数モードの整数の運び手の上限（葉の 2^63。`udf_call` の `EXACT_INT_LEAF_MAX` と一致させる）。
const EXACT_LEAF_MAX: u128 = (1 << 63) - 1;

/// i64::MIN の脱糖形 `0 - 9223372036854775808` の右辺の葉（2^63）の字面。
const MIN_LEAF_DIGITS: &str = "9223372036854775808";

/// 整数モードで `expr` が整数算術・整数比較（丸めなく評価される形）に束縛されるか。
fn binds_to_exact_int(expr: &Expr) -> bool {
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    matches!(
        bind_const_expr_mode(expr, &registry, &mut budget, false),
        Ok((
            BoundExpr::Builtin {
                f: BuiltinFn::ExactIntArith(..) | BuiltinFn::ExactIntCompare(..),
                ..
            },
            _,
            _
        ))
    )
}

/// PG の float8／float4 型の式か（Issue #2881）。内部専用の型付き浮動小数の値、または `+ - * /` の
/// 少なくとも一方がそうである式。PG では float8 との混合算術は float8 で計算されるため、f64 の
/// 結果が正しい意味論になる。型なしの小数リテラル（PG は numeric）・関数・`CASE` 等は偽
/// （従来どおり 2^53 検査で fail-closed）。
fn pg_float_typed(expr: &Expr) -> bool {
    match expr {
        Expr::Call { name, .. } => crate::sql::udf_call::is_typed_float_name(name),
        Expr::Binary { op, lhs, rhs } => {
            matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div)
                && (pg_float_typed(lhs) || pg_float_typed(rhs))
        }
        _ => false,
    }
}

/// 型付き浮動小数との混合式に含まれる型なし小数リテラル（PG では numeric）の精度検査（Issue #2881）。
/// 型付き float 自身の範囲検査は外すが、型なしの小数リテラルを f64 へ丸めて受理しないよう、
/// 非有限または |v| > 2^53 の小数リテラルは `22003`（fail-closed）。型付き値（`Call`）の内側は辿らない。
/// `CASE`／`COALESCE` は `check_intermediate_precision` と同じ短絡評価の到達可能性で、評価される
/// 分岐だけを検査する（未評価分岐の大きなリテラルで偽陽性にしない）。`NULLIF` は両引数を常に評価する。
fn check_untyped_decimal_leaves(expr: &Expr) -> Result<(), SqlSurfaceError> {
    let out_of_range =
        || SqlSurfaceError::numeric_out_of_range("constant expression loses integer precision");
    match expr {
        Expr::Number(n) if !n.bytes().all(|b| b.is_ascii_digit()) => {
            if decimal_literal_exact_within_f64_range(n) {
                Ok(())
            } else {
                Err(out_of_range())
            }
        }
        Expr::Binary { lhs, rhs, .. } | Expr::NullIf(lhs, rhs) => {
            check_untyped_decimal_leaves(lhs)?;
            check_untyped_decimal_leaves(rhs)
        }
        Expr::Coalesce(args) => {
            for a in args {
                if matches!(a, Expr::Null) {
                    continue;
                }
                check_untyped_decimal_leaves(a)?;
                if !matches!(eval_node(a, false), Some(ExprValue::Null)) {
                    break;
                }
            }
            Ok(())
        }
        Expr::Case { whens, else_result } => {
            for (c, r) in whens {
                check_untyped_decimal_leaves(c)?;
                match eval_node(c, false) {
                    Some(ExprValue::Bool(true)) => return check_untyped_decimal_leaves(r),
                    Some(ExprValue::Bool(false) | ExprValue::Null) => {}
                    _ => return Ok(()),
                }
            }
            match else_result {
                Some(e) => check_untyped_decimal_leaves(e),
                None => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// 小数リテラル（先頭 `-` 任意・指数表記可）の厳密な大きさが有限かつ 2^53 以下か。f64 への丸めに
/// 頼らず、仮数の有効桁と指数を十進のまま比較する。解釈できない形（指数が i64 に収まらない等）は
/// 偽（fail-closed）。
fn decimal_literal_exact_within_f64_range(raw: &str) -> bool {
    let body = raw.strip_prefix('-').unwrap_or(raw);
    let (mantissa, exp) = match body.split_once(['e', 'E']) {
        Some((m, e)) => {
            let e = e.strip_prefix('+').unwrap_or(e);
            let Ok(exp) = e.parse::<i64>() else {
                return false;
            };
            (m, exp)
        }
        None => (body, 0),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
        || (int_part.is_empty() && frac_part.is_empty())
    {
        return false;
    }
    let all = format!("{int_part}{frac_part}");
    let lead = all.len() - all.trim_start_matches('0').len();
    let sig = all.trim_start_matches('0').trim_end_matches('0');
    if sig.is_empty() {
        return true;
    }
    // 小数点より左の桁数（仮数の先頭ゼロを除く）。
    let int_digits = (int_part.len() as i64 - lead as i64).saturating_add(exp);
    // 2^53 = 9007199254740992（16 桁）。
    const MAX_DIGITS: &str = "9007199254740992";
    if int_digits <= 15 {
        return true;
    }
    if int_digits > 16 {
        return false;
    }
    let head: String = sig.chars().chain(std::iter::repeat('0')).take(16).collect();
    match head.as_str().cmp(MAX_DIGITS) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Equal => sig.len() <= 16,
        std::cmp::Ordering::Greater => false,
    }
}

/// 整数モードで、`children` のいずれかが整数の運び手（`Numeric`）に評価されるなら `22003`。
/// 運び手は整数算術・整数比較・畳み込み結果にだけ流せ、関数・`NULLIF`・浮動小数との混合・配列等
/// （f64 を前提とする消費者）へ流れる形は精度を保てないため拒否する（fail-closed）。
fn reject_carrier_children<'a>(
    children: impl Iterator<Item = &'a Expr>,
    numeric: bool,
) -> Result<(), SqlSurfaceError> {
    if numeric {
        return Ok(());
    }
    for c in children {
        if matches!(eval_node(c, false), Some(ExprValue::Numeric(_))) {
            return Err(SqlSurfaceError::numeric_out_of_range(
                "constant expression loses integer precision",
            ));
        }
    }
    Ok(())
}

/// NUMERIC 算術モードで `expr` が Numeric 型に束縛されるか（束縛不能は `false`）。
fn binds_to_numeric(expr: &Expr) -> bool {
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    matches!(
        bind_const_expr_mode(expr, &registry, &mut budget, true),
        Ok((_, ExprType::Numeric, _))
    )
}

/// 1 ノードを束縛・評価する（束縛・評価不能は `None`。短絡判定専用）。
fn eval_node(expr: &Expr, numeric: bool) -> Option<ExprValue<'static>> {
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    // 裸の NULL を含む式は型付き NULL へ畳んでから束縛し、短絡判定に使えるようにする。
    let folded = propagate_null(expr, numeric).ok()?;
    let (bound, _ty, _) = bind_const_expr_mode(&folded, &registry, &mut budget, numeric).ok()?;
    eval(&bound, 0, &[]).ok()
}

/// 1 ノードを単独評価し、スカラ結果が 2^53 以上なら `22003`（評価不能は無視）。
fn check_node_value(expr: &Expr, enabled: bool, numeric: bool) -> Result<(), SqlSurfaceError> {
    if !enabled {
        return Ok(());
    }
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    let Ok((bound, _ty, _)) = bind_const_expr_mode(expr, &registry, &mut budget, numeric) else {
        return Ok(());
    };
    if let Ok(ExprValue::Scalar(v)) = eval(&bound, 0, &[]) {
        if float_beyond_fold_range(v) {
            return Err(SqlSurfaceError::numeric_out_of_range(
                "constant expression loses integer precision",
            ));
        }
    }
    Ok(())
}

/// 1 ノードを単独評価し、スカラ結果が非有限なら `22003`（評価不能は無視。float 型の式用。Issue #2881）。
fn check_node_finite(expr: &Expr) -> Result<(), SqlSurfaceError> {
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    let Ok((bound, _ty, _)) = bind_const_expr_mode(expr, &registry, &mut budget, false) else {
        return Ok(());
    };
    if let Ok(ExprValue::Scalar(v)) = eval(&bound, 0, &[]) {
        if !v.is_finite() {
            return Err(SqlSurfaceError::numeric_out_of_range(
                "constant expression result is out of range",
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
        ExprType::TimestampTz => twin(Expr::TimestampTzLiteral {
            micros: 0,
            session_local: false,
        }),
        ExprType::Json | ExprType::JsonText => {
            return Err(SqlSurfaceError::unsupported(
                "a json value is not allowed in a constant expression",
            ))
        }
        // numeric 値は CAST の結果としてのみ生じ、INSERT VALUES の定数式では CAST 自体を拒否する
        // ため到達しない（fail-closed）。
        ExprType::Numeric | ExprType::Uuid | ExprType::Bytea | ExprType::Array => {
            return Err(SqlSurfaceError::unsupported(
                "a value produced by CAST is not allowed in a constant expression",
            ))
        }
        // `INTERVAL` は `NULLIF` が比較を要する（#1580 まで未対応）ため、分岐なしの
        // `CASE WHEN 1 = 0 THEN INTERVAL '0' END` で型付き NULL を表す（Issue #1581）。
        ExprType::Interval => Expr::Case {
            whens: vec![(
                Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Number("1".to_string())),
                    rhs: Box::new(Expr::Number("0".to_string())),
                },
                Expr::IntervalLiteral(crate::datetime::Interval {
                    months: 0,
                    days: 0,
                    micros: 0,
                }),
            )],
            else_result: None,
        },
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
            Expr::TimestampTzLiteral {
                micros: 0,
                session_local: false,
            } => Some(Some(ExprType::TimestampTz)),
            _ => None,
        },
        Expr::Case { whens, else_result } if else_result.is_none() && whens.len() == 1 => {
            if *e == typed_null(ExprType::Bool).ok()? {
                Some(Some(ExprType::Bool))
            } else if *e == typed_null(ExprType::Interval).ok()? {
                Some(Some(ExprType::Interval))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `NULL` オペランドの型候補（`NULL` 側の位置に置いて束縛を試す順序。算術で曖昧な
/// `NULL + 1` は PostgreSQL と同様に Scalar を優先する）。
const NULL_OPERAND_CANDIDATES: [ExprType; 7] = [
    ExprType::Scalar,
    ExprType::Text,
    ExprType::Date,
    ExprType::Timestamp,
    ExprType::Time,
    ExprType::TimestampTz,
    ExprType::Interval,
];

/// 関数呼び出しの `NULL` 引数に対する型候補の組み合わせ試行回数の上限（DoS 防止）。
const MAX_NULL_ARG_PROBES: usize = 256;

/// 1 オペランドが取りうる置換候補。非 `NULL` は式そのもの、型既知の `NULL` は
/// 型付き `NULL`、裸の `NULL` は型候補ごとの型付き `NULL`。
///
/// 裸の `NULL` は `prefer` の型を候補の先頭へ移す（相手オペランドが `INTERVAL` の
/// 加減算で `INTERVAL` 同士の演算を選ばせるため。Issue #1581）。
fn operand_options(e: &Expr, k: Option<Option<ExprType>>, prefer: Option<ExprType>) -> Vec<Expr> {
    match k {
        None => vec![e.clone()],
        Some(Some(t)) => typed_null(t).into_iter().collect(),
        Some(None) => NULL_OPERAND_CANDIDATES
            .iter()
            .filter(|t| Some(**t) == prefer)
            .chain(
                NULL_OPERAND_CANDIDATES
                    .iter()
                    .filter(|t| Some(**t) != prefer),
            )
            .filter_map(|t| typed_null(*t).ok())
            .collect(),
    }
}

/// 非 `NULL` オペランドが `INTERVAL` 型なら、加減算の裸 `NULL` 側で優先する型（`INTERVAL`）を返す。
fn interval_preference(
    op: BinOp,
    other: &Expr,
    other_null: Option<Option<ExprType>>,
    numeric: bool,
) -> Option<ExprType> {
    if other_null.is_some() || !matches!(op, BinOp::Add | BinOp::Sub) {
        return None;
    }
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    match bind_const_expr_mode(other, &registry, &mut budget, numeric) {
        Ok((_, ExprType::Interval, _)) => Some(ExprType::Interval),
        _ => None,
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
    numeric: bool,
) -> Result<Expr, SqlSurfaceError> {
    let mut first_err: Option<SqlSurfaceError> = None;
    let lpref = interval_preference(op, r, rn, numeric);
    let rpref = interval_preference(op, l, ln, numeric);
    for lhs in operand_options(l, ln, lpref) {
        for rhs in operand_options(r, rn, rpref) {
            let probe = Expr::Binary {
                op,
                lhs: Box::new(lhs.clone()),
                rhs: Box::new(rhs),
            };
            let registry = UdfRegistry::default();
            let mut budget = MAX_EXPR_NODES;
            match bind_const_expr_mode(&probe, &registry, &mut budget, numeric) {
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
fn fold_null_call(name: &str, args: Vec<Expr>, numeric: bool) -> Result<Expr, SqlSurfaceError> {
    let try_bind = |args: &[Expr]| {
        let probe = Expr::Call {
            name: name.to_string(),
            args: args.to_vec(),
        };
        let registry = UdfRegistry::default();
        let mut budget = MAX_EXPR_NODES;
        bind_const_expr_mode(&probe, &registry, &mut budget, numeric).map(|_| probe)
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
        .map(|a| operand_options(a, null_kind(a), None))
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
fn propagate_null(expr: &Expr, numeric: bool) -> Result<Expr, SqlSurfaceError> {
    Ok(match expr {
        Expr::Binary { op, lhs, rhs } => {
            let l = propagate_null(lhs, numeric)?;
            let r = propagate_null(rhs, numeric)?;
            let (ln, rn) = (null_kind(&l), null_kind(&r));
            if ln.is_some() || rn.is_some() {
                fold_null_binary(*op, &l, &r, ln, rn, numeric)?
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
            args.iter()
                .map(|a| propagate_null(a, numeric))
                .collect::<Result<_, _>>()?,
            numeric,
        )?,
        Expr::Coalesce(args) => Expr::Coalesce(
            args.iter()
                .map(|a| propagate_null(a, numeric))
                .collect::<Result<_, _>>()?,
        ),
        Expr::NullIf(l, r) => Expr::NullIf(
            Box::new(propagate_null(l, numeric)?),
            Box::new(propagate_null(r, numeric)?),
        ),
        Expr::Case { whens, else_result } => Expr::Case {
            whens: whens
                .iter()
                .map(|(c, r)| Ok((propagate_null(c, numeric)?, propagate_null(r, numeric)?)))
                .collect::<Result<_, SqlSurfaceError>>()?,
            else_result: match else_result {
                Some(e) => Some(Box::new(propagate_null(e, numeric)?)),
                None => None,
            },
        },
        // 配列コンストラクタ・添字（TABLE-35・Issue #1602）。要素・添字の中の `NULL` を含む
        // 演算だけを型付き `NULL` へ畳み、裸の `NULL` 要素はそのまま残す（束縛器が共通型へ解決する）。
        Expr::ArrayConstructor(items) => Expr::ArrayConstructor(
            items
                .iter()
                .map(|a| propagate_null(a, numeric))
                .collect::<Result<_, _>>()?,
        ),
        Expr::Subscript { base, subscript } => {
            use crate::sql::udf_call::ArraySubscript;
            let opt = |b: &Option<Box<Expr>>| -> Result<Option<Box<Expr>>, SqlSurfaceError> {
                b.as_deref()
                    .map(|e| propagate_null(e, numeric).map(Box::new))
                    .transpose()
            };
            Expr::Subscript {
                base: Box::new(propagate_null(base, numeric)?),
                subscript: match subscript {
                    ArraySubscript::Index(i) => {
                        ArraySubscript::Index(Box::new(propagate_null(i, numeric)?))
                    }
                    ArraySubscript::Slice { lo, hi } => ArraySubscript::Slice {
                        lo: opt(lo)?,
                        hi: opt(hi)?,
                    },
                },
            }
        }
        other => other.clone(),
    })
}

/// 列参照を含まない式を評価して [`InsertLiteral`] へ畳み込む。
pub(crate) fn fold_const_literal(expr: &Expr) -> Result<InsertLiteral, SqlSurfaceError> {
    prescan(expr)?;
    // Issue #2799: 内部専用の型付き NUMERIC 値を含む式だけ NUMERIC 算術モードで束縛する。
    // 含まない式の束縛経路は従来と同一（int・float の畳み込みの挙動を変えない）。
    let numeric = contains_typed_numeric(expr);
    if first_non_builtin_call_with(expr, false, true).is_some() {
        return Err(SqlSurfaceError::unsupported(
            "only built-in functions are allowed in a constant expression",
        ));
    }
    // 最終評価は NULL 伝播（型付き NULL への畳み込み）後の式で行う。
    // 精度検査は NULL 畳み込み前の元の式に対し短絡評価で選ばれた枝だけで行う
    // （畳み込みは型検証専用。未評価枝の精度欠落を 22003 にしない）。
    check_intermediate_precision(expr, numeric)?;
    let propagated = propagate_null(expr, numeric)?;
    if matches!(propagated, Expr::Null) {
        return Ok(InsertLiteral::Null);
    }
    let expr = &propagated;
    let registry = UdfRegistry::default();
    let mut budget = MAX_EXPR_NODES;
    let (bound, _ty, _) = bind_const_expr_array_top_mode(expr, &registry, &mut budget, numeric)?;
    if crate::sql::udf_call::references_embedding(&bound) || matches!(bound, BoundExpr::IdRef) {
        return Err(SqlSurfaceError::Internal {
            detail: "constant expression resolved to a row reference".to_string(),
        });
    }
    match eval(&bound, 0, &[])? {
        // Issue #2881: PG の float8／float4 型の式の結果は 2^53 以上でも有限なら受理する。
        // 整数値は |v| < 2^63 なら整数表記（BIGINT 列へも正確に入る）、それ以上は `{v:?}` の
        // 指数表記（`as i64` の飽和で誤値にしない）。
        ExprValue::Scalar(v) if !numeric && v.is_finite() && pg_float_typed(expr) => {
            if v.fract() != 0.0 {
                Ok(InsertLiteral::Number(format!("{v}")))
            } else if v.abs() < 9_223_372_036_854_775_808.0 {
                Ok(InsertLiteral::Number(format!("{}", v as i64)))
            } else {
                Ok(InsertLiteral::Number(format!("{v:?}")))
            }
        }
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
        // 代入先の列型が決まる束縛段で変換するため型を保持する（Issue #1576）。
        ExprValue::TimestampTz(t) => Ok(InsertLiteral::TimestampTz(t)),
        // INTERVAL 列へは `parse_interval` で再解析されて入る（正規形の往復一致は単体テストで固定）。
        ExprValue::Interval(v) => Ok(InsertLiteral::String(crate::datetime::format_interval(&v))),
        ExprValue::Vector(_) => Err(SqlSurfaceError::Internal {
            detail: "constant expression produced a vector".to_string(),
        }),
        ExprValue::Json(_) | ExprValue::JsonText(_) => Err(SqlSurfaceError::unsupported(
            "a json value is not allowed in a constant expression",
        )),
        // 配列（TABLE-35・Issue #1602）は PG 配列テキストへ畳み、列側の `parse_array_literal` が
        // 列の要素型で解釈し直す（Date／Interval と同じ前例。エンコード済みの `InsertLiteral` を
        // 増やさないため content_hash・DDL・wire は不変）。往復は `ArrayValue::to_pg_text` の契約。
        ExprValue::Array(a) => a
            .to_pg_text_bounded(crate::sql::parser::MAX_ARRAY_LITERAL_BYTES)
            .map(InsertLiteral::String)
            .ok_or_else(|| {
                SqlSurfaceError::payload_too_large(
                    "constant array expression exceeds the maximum array literal length",
                )
            }),
        // Issue #2799: NUMERIC 値は正規テキストの数値リテラルへ戻し、列側のリテラル束縛が列型で
        // 解釈し直す（Date・Interval と同じ前例。`InsertLiteral` を増やさないため content_hash・
        // DDL・wire は不変）。整数値は整数表記へ正規化する（`4 / 2` が INTEGER 列へ入る）。
        // `numeric` モードでしか生成されない（`CAST` は `prescan` が拒否済み）。
        // 整数モードの Numeric は整数の運び手（scale 0）で、bigint の範囲外は `22003`（Issue #2881）。
        ExprValue::Numeric(d) => {
            if !numeric && i64::try_from(d.unscaled()).is_err() {
                return Err(SqlSurfaceError::numeric_out_of_range("bigint out of range"));
            }
            numeric_literal_text(&d).map(InsertLiteral::Number)
        }
        ExprValue::Uuid(_) | ExprValue::Bytes(_) => Err(SqlSurfaceError::unsupported(
            "a value produced by CAST is not allowed in a constant expression",
        )),
    }
}

/// NUMERIC 値の数値リテラルテキスト。小数部がすべて 0 なら整数表記（f64 側の畳み込みと同じ規則）、
/// それ以外は `Decimal` の正規テキスト（scale を保つ）。
fn numeric_literal_text(d: &crate::numeric::Decimal) -> Result<String, SqlSurfaceError> {
    let internal = || SqlSurfaceError::Internal {
        detail: "numeric scale out of range".to_string(),
    };
    let divisor = 10i128
        .checked_pow(u32::from(d.scale()))
        .ok_or_else(internal)?;
    if d.unscaled().checked_rem(divisor) == Some(0) {
        let int = d.unscaled().checked_div(divisor).ok_or_else(internal)?;
        Ok(int.to_string())
    } else {
        Ok(d.to_string())
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

    /// INTERVAL の畳み込み結果は `format_interval` → `parse_interval` で往復一致する
    /// （INTERVAL 列へは再解析されて入るため、極値でも値が変わらないこと。Issue #1581）。
    #[test]
    fn folded_interval_literal_roundtrips_through_text() {
        use crate::datetime::{format_interval, parse_interval, Interval};
        for v in [
            Interval {
                months: i32::MAX,
                days: i32::MAX,
                micros: i64::MAX - 1,
            },
            Interval {
                months: i32::MIN,
                days: i32::MIN,
                micros: i64::MIN + 1,
            },
            Interval {
                months: -1,
                days: 1,
                micros: -1,
            },
            Interval {
                months: 0,
                days: 0,
                micros: 0,
            },
            Interval {
                months: 14,
                days: -3,
                micros: 4_000_000,
            },
        ] {
            let InsertLiteral::String(text) =
                fold_const_literal(&Expr::IntervalLiteral(v)).expect("fold")
            else {
                panic!("expected a string literal");
            };
            assert_eq!(parse_interval(&text), Ok(v), "{text}");
            assert_eq!(format_interval(&v), text);
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
        // Issue #2881: 2^53 を超える整数の算術は丸めずに評価する（bigint 全域）。
        assert_eq!(
            fold_text(&bin(BinOp::Mul, num("9007199254740993"), num("2"))),
            "18014398509481986"
        );
        let big = bin(BinOp::Add, num("9007199254740992"), num("1"));
        assert_eq!(
            fold_text(&bin(BinOp::Sub, big, num("9007199254740992"))),
            "1"
        );
        assert_eq!(
            fold_text(&bin(BinOp::Add, num("9007199254740993"), num("0"))),
            "9007199254740993"
        );
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
        let big = call("power", vec![num("2"), num("54")]);
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

    // ---- Issue #2799: 型付き NUMERIC 値の算術（SQL-24・SQL-68 ポインタ） ----

    fn n(raw: &str) -> Expr {
        crate::sql::udf_call::typed_value_expr("$typed:numeric", raw.to_string())
    }
    fn fold_text(e: &Expr) -> String {
        match fold_const_literal(e).expect("fold") {
            InsertLiteral::Number(t) => t,
            other => panic!("unexpected literal {other:?}"),
        }
    }
    fn fold_code(e: &Expr) -> &'static str {
        fold_const_literal(e).expect_err("must fail").wire_code()
    }

    #[test]
    fn numeric_arith_keeps_pg_scale_rules() {
        // PostgreSQL: 3::numeric / 2 = 1.5000000000000000
        assert_eq!(
            fold_text(&bin(BinOp::Div, n("3"), num("2"))),
            "1.5000000000000000"
        );
        // 割り切れる値は整数表記へ正規化する。
        assert_eq!(fold_text(&bin(BinOp::Div, n("4"), num("2"))), "2");
        assert_eq!(fold_text(&bin(BinOp::Add, n("1.25"), num("1"))), "2.25");
        assert_eq!(fold_text(&bin(BinOp::Add, n("0.1"), num("0.2"))), "0.3");
        assert_eq!(fold_text(&bin(BinOp::Mul, n("-3"), num("2"))), "-6");
        assert_eq!(fold_text(&bin(BinOp::Add, num("1"), n("1.5"))), "2.5");
        assert_eq!(
            fold_text(&bin(BinOp::Add, n("100000000000000000000"), num("1"))),
            "100000000000000000001"
        );
        assert_eq!(
            fold_text(&bin(
                BinOp::Add,
                bin(BinOp::Div, n("3"), num("2")),
                num("1")
            )),
            "2.5000000000000000"
        );
    }

    #[test]
    fn numeric_arith_errors_and_nulls() {
        assert_eq!(fold_code(&bin(BinOp::Div, n("1"), num("0"))), "22012");
        let big = "9".repeat(38);
        assert_eq!(fold_code(&bin(BinOp::Mul, n(&big), num("10"))), "22003");
        let r = fold_const_literal(&bin(BinOp::Div, n("3"), Expr::Null));
        assert!(matches!(r, Ok(InsertLiteral::Null)), "{r:?}");
        assert_eq!(
            fold_text(&bin(
                BinOp::Div,
                Expr::Coalesce(vec![Expr::Null, n("3")]),
                num("2")
            )),
            "1.5000000000000000"
        );
        let case = Expr::Case {
            whens: vec![(
                bin(BinOp::Gt, num("1"), num("0")),
                bin(BinOp::Add, n("3"), num("1")),
            )],
            else_result: Some(Box::new(num("1"))),
        };
        assert_eq!(fold_text(&case), "4");
    }

    #[test]
    fn numeric_arith_mixing_is_fail_closed() {
        let f8 = crate::sql::udf_call::typed_value_expr("$typed:float8", "1.5".to_string());
        assert_eq!(fold_code(&bin(BinOp::Add, f8, n("1"))), "0A000");
        let i4 = crate::sql::udf_call::typed_value_expr("$typed:int4", "2".to_string());
        assert_eq!(fold_text(&bin(BinOp::Add, i4, n("1.5"))), "3.5");
        let code = fold_code(&call("abs", vec![n("3")]));
        assert!(matches!(code, "0A000" | "42883"), "{code}");
    }

    #[test]
    fn numeric_context_big_literal_under_non_binary_parent_is_fail_closed() {
        // 二項演算の直下以外の巨大整数リテラルは束縛側が f64 で受けるため、numeric 文脈でも
        // 非 numeric 文脈でも 22003 で拒否する（fail-closed。誤った丸め値を返さない）。
        let big = "100000000000000000000";
        let case = Expr::Case {
            whens: vec![(bin(BinOp::Gt, num("1"), num("0")), num(big))],
            else_result: Some(Box::new(num("1"))),
        };
        assert_eq!(fold_code(&bin(BinOp::Add, n("1"), case.clone())), "22003");
        assert_eq!(fold_code(&case), "22003");
        let co = Expr::Coalesce(vec![Expr::Null, num(big)]);
        assert_eq!(fold_code(&bin(BinOp::Add, n("1"), co.clone())), "22003");
        assert_eq!(fold_code(&co), "22003");
    }

    #[test]
    fn expressions_without_typed_numeric_are_unchanged() {
        assert_eq!(fold_text(&bin(BinOp::Div, num("7"), num("2"))), "3");
        assert_eq!(fold_text(&bin(BinOp::Mul, num("1.5"), num("2"))), "3");
        assert_eq!(
            fold_text(&bin(BinOp::Add, num("0.1"), num("0.2"))),
            "0.30000000000000004"
        );
    }

    fn code(e: &Expr) -> &'static str {
        fold_code(e)
    }

    fn typed(ty: &str, raw: &str) -> Expr {
        crate::sql::udf_call::typed_value_expr(&format!("$typed:{ty}"), raw.to_string())
    }

    #[test]
    fn exact_int_arith_covers_bigint_range() {
        let i64max = "9223372036854775807";
        assert_eq!(
            fold_text(&bin(BinOp::Mul, num("4000000000000000"), num("4"))),
            "16000000000000000"
        );
        assert_eq!(
            fold_text(&bin(BinOp::Div, num("9007199254740993"), num("2"))),
            "4503599627370496"
        );
        assert_eq!(
            fold_text(&bin(BinOp::Sub, num(i64max), num("1"))),
            "9223372036854775806"
        );
        assert_eq!(code(&bin(BinOp::Add, num(i64max), num("1"))), "22003");
        let min = || bin(BinOp::Sub, num("0"), num("9223372036854775808"));
        assert_eq!(fold_text(&min()), "-9223372036854775808");
        assert_eq!(code(&bin(BinOp::Sub, min(), num("1"))), "22003");
        assert_eq!(
            code(&bin(BinOp::Div, min(), bin(BinOp::Sub, num("0"), num("1")))),
            "22003"
        );
        // 2^63 の葉が単独で残る形・2^63 を超える葉は範囲外。
        assert_eq!(
            code(&bin(BinOp::Add, num("9223372036854775809"), num("0"))),
            "22003"
        );
        assert_eq!(code(&num("9223372036854775808")), "22003");
        // 2^63 の葉は i64::MIN の脱糖形の右辺以外では拒否する（bigint の切り捨てで別の整数にしない）。
        for e in [
            bin(BinOp::Div, num("9223372036854775808"), num("3")),
            bin(BinOp::Sub, num("1"), num("9223372036854775808")),
            bin(
                BinOp::Sub,
                num("0"),
                bin(BinOp::Div, num("9223372036854775808"), num("3")),
            ),
        ] {
            assert_eq!(code(&e), "22003");
        }
        // 従来どおり int4 の溢れ・0 除算。
        assert_eq!(code(&bin(BinOp::Add, num("2147483647"), num("1"))), "22003");
        assert_eq!(
            code(&bin(BinOp::Div, num("9007199254740993"), num("0"))),
            "22012"
        );
        assert_eq!(fold_text(&bin(BinOp::Div, num("7"), num("2"))), "3");
    }

    #[test]
    fn exact_int_comparison_is_exact() {
        let cmp = |op, l: &str, r: &str| fold_const_literal(&bin(op, num(l), num(r))).unwrap();
        assert_eq!(
            cmp(BinOp::Eq, "9007199254740993", "9007199254740992"),
            InsertLiteral::Bool(false)
        );
        assert_eq!(
            cmp(BinOp::Gt, "9007199254740993", "9007199254740992"),
            InsertLiteral::Bool(true)
        );
        assert_eq!(
            cmp(BinOp::Le, "9007199254740993", "9007199254740993"),
            InsertLiteral::Bool(true)
        );
    }

    #[test]
    fn exact_int_carrier_does_not_flow_into_f64_consumers() {
        let big = || num("9007199254740993");
        for e in [
            call("abs", vec![big()]),
            call(
                "abs",
                vec![bin(BinOp::Mul, num("4000000000000000"), num("4"))],
            ),
            bin(BinOp::Add, big(), num("1.5")),
            Expr::NullIf(Box::new(big()), Box::new(num("1"))),
            bin(
                BinOp::Add,
                Expr::Case {
                    whens: vec![(bin(BinOp::Gt, num("1"), num("0")), big())],
                    else_result: Some(Box::new(num("1"))),
                },
                num("1.5"),
            ),
        ] {
            assert_eq!(code(&e), "22003", "{e:?}");
        }
        // 値としてそのまま返る形は正確に畳み込む。
        let coalesce = Expr::Coalesce(vec![Expr::Null, big()]);
        assert_eq!(fold_text(&coalesce), "9007199254740993");
    }

    #[test]
    fn exact_typed_values_and_float_expressions() {
        assert_eq!(
            fold_text(&bin(
                BinOp::Add,
                typed("int8", "9007199254740992"),
                num("1")
            )),
            "9007199254740993"
        );
        assert_eq!(
            fold_text(&bin(BinOp::Add, typed("float8", "1e300"), num("0"))),
            "1e300"
        );
        assert_eq!(
            fold_text(&bin(
                BinOp::Add,
                typed("float8", "9007199254740994.0"),
                num("0")
            )),
            "9007199254740994"
        );
        assert_eq!(
            code(&bin(
                BinOp::Mul,
                typed("float8", "1e300"),
                typed("float8", "1e300")
            )),
            "22003"
        );
        assert_eq!(
            code(&bin(BinOp::Add, typed("float4", "1e300"), num("0"))),
            "22003"
        );
        // 型なしの小数リテラルは PG では numeric。f64 へ丸めて返さない。
        assert_eq!(
            code(&bin(BinOp::Add, num("9007199254740993.5"), num("0"))),
            "22003"
        );
        // 型付き float との混合でも、型なしの小数リテラルの精度検査は維持する。
        assert_eq!(
            code(&bin(
                BinOp::Add,
                typed("float8", "0"),
                num("9007199254740993.5")
            )),
            "22003"
        );
        // 整数の運び手を float 型の算術へ流す形は拒否する。
        assert_eq!(
            code(&bin(
                BinOp::Add,
                typed("float8", "1.5"),
                num("9007199254740993")
            )),
            "22003"
        );
    }

    /// Issue #2881 のレビュー指摘: 型なし小数リテラルの精度検査は評価される分岐だけに適用し、
    /// 指数表記は十進のまま厳密に判定し、NULLIF の引数も検査する。
    #[test]
    fn untyped_decimal_leaf_check_respects_reachability_exponent_and_nullif() {
        let big = || num("9007199254740993.5");
        let f0 = || typed("float8", "0");
        // COALESCE の未評価引数（先頭が非 NULL）の大きな小数は検査しない。
        assert!(fold_const_literal(&bin(
            BinOp::Add,
            f0(),
            Expr::Coalesce(vec![num("1.0"), big()])
        ))
        .is_ok());
        // 評価される引数は検査する。
        assert_eq!(
            code(&bin(
                BinOp::Add,
                f0(),
                Expr::Coalesce(vec![Expr::Null, big()])
            )),
            "22003"
        );
        // CASE の未選択結果は検査せず、選択される分岐は検査する。
        let case = |cond_true: bool| Expr::Case {
            whens: vec![(
                bin(BinOp::Eq, num("1"), num(if cond_true { "1" } else { "2" })),
                big(),
            )],
            else_result: Some(Box::new(num("1.0"))),
        };
        assert!(fold_const_literal(&bin(BinOp::Add, f0(), case(false))).is_ok());
        assert_eq!(code(&bin(BinOp::Add, f0(), case(true))), "22003");
        // 指数表記は f64 丸め前に厳密判定する（2^53 をわずかに超える値も拒否）。
        assert_eq!(
            code(&bin(BinOp::Add, f0(), num("9007199254740992.5e0"))),
            "22003"
        );
        assert_eq!(
            code(&bin(BinOp::Add, f0(), num("90071992547409925e-1"))),
            "22003"
        );
        assert_eq!(code(&bin(BinOp::Add, f0(), num("1e400"))), "22003");
        assert!(fold_const_literal(&bin(BinOp::Add, f0(), num("9007199254740992.0e0"))).is_ok());
        assert!(fold_const_literal(&bin(BinOp::Add, f0(), num("1.5e3"))).is_ok());
        // NULLIF 内の大きな型なし小数も 22003（丸めて保存しない）。
        assert_eq!(
            code(&bin(
                BinOp::Add,
                f0(),
                Expr::NullIf(Box::new(big()), Box::new(num("1.0")))
            )),
            "22003"
        );
    }
}
