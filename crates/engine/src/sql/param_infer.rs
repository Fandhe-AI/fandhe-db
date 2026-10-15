//! 式内 `$n` の型推論（Issue #1546・SQL-45 期待欄 (6)・WIRE-12 のポインタ）。
//!
//! 責務境界: `sql::params` が切り出した式セグメント（`UPDATE ... SET` 右辺、
//! `INSERT ... VALUES` 要素のうち `$n` 単独ではないもの）を構文木へ解析し、
//! 各 `$n` の型を「演算の相手・組み込み関数の引数・`CASE`／`COALESCE`／`NULLIF` の
//! 兄弟枝」から決める。呼び出し元は `core.rs::EngineCore::parse_sql_prepared`（Parse 段）で、
//! 結果は `ParameterDescription` の OID と Bind 時の置換リテラル種別
//! （`sql::params::ParamLiteralKind`）の決定に使われる。値の検証意味論には関与しない。
//!
//! 構文木の取得は既存の `sql::allowlist::Parser`
//! （[`crate::sql::allowlist::parse_param_inference_expr`]）を再利用する（第 2 のパーサーを
//! 作らない）。`$n` はパーサーに渡す前に、字句解析器が生成し得ない擬似識別子（`$` 始まり）へ
//! 置き換える。通常の識別子と衝突しないため、`Expr` に専用 variant を足さずに済み、
//! 実行・束縛・評価の経路へ `$n` 由来の構文木が入る余地もない。
//!
//! ## 推論規則（部分集合。それ以外の形は `42P18` で fail-closed）
//!
//! - 右辺・要素の全体が括弧付きの `$n` 単独: 代入先の列の型。
//! - `<A> op $n` / `$n op <A>`（`+ - * / = < > <= >=`）: 相手 `<A>` の型。列参照は列の型、
//!   整数リテラルは `INTEGER`（i32 に収まらなければ `BIGINT`）、小数・指数リテラルは
//!   `DOUBLE PRECISION`（PostgreSQL の numeric との既知の差分）。
//! - 組み込み関数の引数: テキスト引数なら `TEXT`。数値引数（`abs($1)` 等）は `42P18`
//!   （PostgreSQL の数値オーバーロードの曖昧さに倣う fail-closed）。登録 UDF など
//!   組み込みでない関数の引数は型を引かず `Text` へ倒す（関数の存在を応答コードで
//!   観測させない）。
//! - `CASE` の THEN／ELSE・`COALESCE`・`NULLIF` の `$n`: 兄弟のうち型が決まるものの型。
//!   どこからも決まらない（`$1 + $2`・`COALESCE($1, $2)`）場合は `42P18`。
//!
//! 代入先の列の型は式の内側へ押し下げない（PostgreSQL と同じく演算の相手から決める）。
//! `42P18` にするかどうかは**構文の形だけ**で決め、表・列がスキーマに無い場合は
//! 「未解決」として `Text` へ倒す（表・列の存在で Parse の応答コードが変わる経路を
//! 作らない。未知の列は既存の束縛段のエラーで返る）。
//!
//! 再帰は `Parser` が課すノード予算・深さ上限で抑えられた木だけを走査するため、
//! 本モジュール固有の深さ上限は持たない。

use crate::catalog::ColumnType;
use crate::sql::allowlist::{self, SqlSurfaceError};
use crate::sql::exec::ColumnMeta;
use crate::sql::lexer::Token;
use crate::sql::params::{ExprSegment, PreparedParamType};
use crate::sql::udf_call::{is_builtin_function_name, BinOp, Expr};

/// 1 つの `$n` 出現に対する推論結果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ParamInference {
    /// 型が決まった（式の中で扱える型に限る）。
    Known(PreparedParamType),
    /// 相手が列参照・UDF 等でスキーマから引けず、型を確定できない（`Text` へ倒す。
    /// `42P18` にはしない）。
    Unresolved,
    /// 文脈のどこからも型を決められない（全出現がこれなら `42P18`）。
    Indeterminate,
}

/// 部分式の型。
#[derive(Debug, Clone, PartialEq)]
enum Ty {
    Known(PreparedParamType),
    /// 列参照・UDF 呼び出し等、スキーマ側の都合で確定できないもの。
    Unresolved,
    /// `$n` 自身・`NULL`・比較結果など、型を持たない（兄弟から決める対象）。
    Untyped,
}

/// 式の中で扱える `$n` の型か（置換後のリテラルが式の文脈でその型として束縛される
/// 整数・浮動小数・テキストに限る）。
fn is_supported_in_expr(ty: &PreparedParamType) -> bool {
    match ty {
        PreparedParamType::Column(ColumnMeta::Id) => true,
        PreparedParamType::Column(ColumnMeta::Scalar { ty, .. }) => matches!(
            ty,
            ColumnType::SmallInt
                | ColumnType::Integer
                | ColumnType::BigInt
                | ColumnType::Real
                | ColumnType::Double
                | ColumnType::Text
                | ColumnType::Varchar
        ),
        PreparedParamType::Text => true,
        PreparedParamType::VectorText | PreparedParamType::Column(_) => false,
    }
}

fn scalar(name: &str, ty: ColumnType) -> PreparedParamType {
    PreparedParamType::Column(ColumnMeta::Scalar {
        name: name.to_string(),
        ty,
    })
}

/// `$n` の擬似識別子名（字句解析器は `$` 始まりの識別子を生成しない）。
fn placeholder_name(n: u16) -> String {
    format!("${n}")
}

/// 擬似識別子から `$n` の番号を取り出す。
fn as_param(expr: &Expr) -> Option<u16> {
    match expr {
        Expr::Ident(name) => name.strip_prefix('$')?.parse::<u16>().ok(),
        _ => None,
    }
}

/// 数値リテラルの型（整数は i32 に収まれば `INTEGER`、収まらなければ `BIGINT`、
/// 小数・指数・i64 超過は `DOUBLE PRECISION`）。
fn number_type(raw: &str) -> PreparedParamType {
    let is_float_shape = raw.contains(['.', 'e', 'E']);
    if !is_float_shape {
        if raw.parse::<i32>().is_ok() {
            return scalar("?column?", ColumnType::Integer);
        }
        if raw.parse::<i64>().is_ok() {
            return scalar("?column?", ColumnType::BigInt);
        }
    }
    scalar("?column?", ColumnType::Double)
}

/// 数値型の昇格（浮動小数があれば `DOUBLE`、整数同士は幅の広い方）。数値以外が
/// 混ざる場合は左辺の型を返す（食い違いは束縛段が報告する）。
fn promote(lhs: PreparedParamType, rhs: PreparedParamType) -> PreparedParamType {
    fn rank(ty: &PreparedParamType) -> Option<u8> {
        match ty {
            PreparedParamType::Column(ColumnMeta::Id) => Some(2),
            PreparedParamType::Column(ColumnMeta::Scalar { ty, .. }) => match ty {
                ColumnType::SmallInt => Some(0),
                ColumnType::Integer => Some(1),
                ColumnType::BigInt => Some(2),
                ColumnType::Real => Some(3),
                ColumnType::Double => Some(4),
                _ => None,
            },
            _ => None,
        }
    }
    // 対象外型（DATE／TIMESTAMP の `VectorText`）は昇格で落とさず伝播させ、
    // 入れ子の算術でも `assign` の `0A000` に到達させる。
    if lhs == PreparedParamType::VectorText || rhs == PreparedParamType::VectorText {
        return PreparedParamType::VectorText;
    }
    match (rank(&lhs), rank(&rhs)) {
        (Some(l), Some(r)) if r > l => rhs,
        _ => lhs,
    }
}

fn is_comparison(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le | BinOp::Eq
    )
}

/// 組み込み関数の引数位置が受け取る型。`None` は数値など推論対象外（`42P18`）。
fn builtin_arg_is_text(name: &str, index: usize) -> bool {
    match name.to_ascii_lowercase().as_str() {
        "lower" | "upper" | "length" | "trim" => index == 0,
        "substr" => index == 0,
        "replace" | "position" | "concat" => true,
        _ => false,
    }
}

/// 組み込み関数の戻り値の型（`$n` の相手として使うときの近似）。
fn builtin_return_type(name: &str) -> Ty {
    match name.to_ascii_lowercase().as_str() {
        "lower" | "upper" | "trim" | "replace" | "concat" | "substr" => {
            Ty::Known(PreparedParamType::Text)
        }
        "length" | "position" => Ty::Known(scalar("?column?", ColumnType::Integer)),
        _ if is_builtin_function_name(name) => Ty::Known(scalar("?column?", ColumnType::Double)),
        _ => Ty::Unresolved,
    }
}

struct Inferrer<'a> {
    /// 代入先表の列名から列の型を引く。表・列が引けなければ `None`。
    column_type: &'a dyn Fn(&str) -> Option<PreparedParamType>,
    out: Vec<(u16, ParamInference)>,
}

impl Inferrer<'_> {
    fn type_of(&self, expr: &Expr) -> Ty {
        match expr {
            Expr::Number(raw) => Ty::Known(number_type(raw)),
            Expr::String(_) => Ty::Known(PreparedParamType::Text),
            Expr::Ident(name) => {
                if as_param(expr).is_some() {
                    return Ty::Untyped;
                }
                // 実列名を先に引く（列名が `null`／`true`／`false` と衝突しても列として扱う）。
                // 列に解決できない予約語リテラルだけを型なしとして扱う。
                if let Some(ty) = (self.column_type)(name) {
                    return Ty::Known(ty);
                }
                if name.eq_ignore_ascii_case("null")
                    || name.eq_ignore_ascii_case("true")
                    || name.eq_ignore_ascii_case("false")
                {
                    return Ty::Untyped;
                }
                Ty::Unresolved
            }
            Expr::Call { name, .. } => builtin_return_type(name),
            Expr::Binary { op, lhs, rhs } => {
                if is_comparison(*op) {
                    return Ty::Untyped;
                }
                match (self.type_of(lhs), self.type_of(rhs)) {
                    (Ty::Known(l), Ty::Known(r)) => Ty::Known(promote(l, r)),
                    (Ty::Unresolved, _) | (_, Ty::Unresolved) => Ty::Unresolved,
                    (Ty::Known(t), Ty::Untyped) | (Ty::Untyped, Ty::Known(t)) => Ty::Known(t),
                    (Ty::Untyped, Ty::Untyped) => Ty::Untyped,
                }
            }
            Expr::Case { whens, else_result } => {
                let branches = whens
                    .iter()
                    .map(|(_, r)| r)
                    .chain(else_result.as_deref())
                    .collect::<Vec<_>>();
                self.sibling_type(branches.into_iter())
            }
            Expr::Coalesce(args) => self.sibling_type(args.iter()),
            Expr::NullIf(a, b) => self.sibling_type([a.as_ref(), b.as_ref()].into_iter()),
            Expr::Null => Ty::Untyped,
            // DATE／TIMESTAMP は式内 `$n` の対象外型。`Text` へ倒さず `assign` の
            // 対象外型判定（`0A000`）に落として fail-closed にする。
            Expr::DateLiteral(_) | Expr::TimestampLiteral(_) => {
                Ty::Known(PreparedParamType::VectorText)
            }
        }
    }

    /// 兄弟式のうち型が決まるもの（最初の `Known`）。決まらず未解決があれば `Unresolved`。
    fn sibling_type<'e>(&self, exprs: impl Iterator<Item = &'e Expr>) -> Ty {
        let mut unresolved = false;
        let mut first: Option<PreparedParamType> = None;
        for e in exprs {
            match self.type_of(e) {
                // 対象外型は他の兄弟より優先して伝播させる（`0A000` で fail-closed）。
                Ty::Known(PreparedParamType::VectorText) => {
                    return Ty::Known(PreparedParamType::VectorText)
                }
                Ty::Known(t) => {
                    first.get_or_insert(t);
                }
                Ty::Unresolved => unresolved = true,
                Ty::Untyped => {}
            }
        }
        if let Some(t) = first {
            return Ty::Known(t);
        }
        if unresolved {
            Ty::Unresolved
        } else {
            Ty::Untyped
        }
    }

    /// `$n` に型 `ty` を割り当てる。式の中で扱えない型は `0A000` で fail-closed。
    fn assign(&mut self, n: u16, ty: Ty) -> Result<(), SqlSurfaceError> {
        let result = match ty {
            Ty::Known(t) => {
                if !is_supported_in_expr(&t) {
                    return Err(SqlSurfaceError::FeatureNotSupported {
                        detail: format!(
                            "parameter ${n} of this data type is not supported inside an expression"
                        ),
                    });
                }
                ParamInference::Known(t)
            }
            Ty::Unresolved => ParamInference::Unresolved,
            Ty::Untyped => ParamInference::Indeterminate,
        };
        self.out.push((n, result));
        Ok(())
    }

    fn walk(&mut self, expr: &Expr) -> Result<(), SqlSurfaceError> {
        match expr {
            Expr::Binary { lhs, rhs, .. } => {
                // どちらかの辺が `$n` 単独なら、相手の型を割り当てる。
                match (as_param(lhs), as_param(rhs)) {
                    (Some(a), Some(b)) => {
                        self.assign(a, Ty::Untyped)?;
                        self.assign(b, Ty::Untyped)?;
                    }
                    (Some(a), None) => {
                        let other = self.type_of(rhs);
                        self.assign(a, other)?;
                        self.walk(rhs)?;
                    }
                    (None, Some(b)) => {
                        let other = self.type_of(lhs);
                        self.assign(b, other)?;
                        self.walk(lhs)?;
                    }
                    (None, None) => {
                        self.walk(lhs)?;
                        self.walk(rhs)?;
                    }
                }
            }
            Expr::Call { name, args } => {
                for (i, arg) in args.iter().enumerate() {
                    match as_param(arg) {
                        Some(n) => {
                            let ty = if !is_builtin_function_name(name) {
                                Ty::Unresolved
                            } else if builtin_arg_is_text(name, i) {
                                Ty::Known(PreparedParamType::Text)
                            } else {
                                Ty::Untyped
                            };
                            self.assign(n, ty)?;
                        }
                        None => self.walk(arg)?,
                    }
                }
            }
            Expr::Case { whens, else_result } => {
                for (cond, _) in whens {
                    self.walk(cond)?;
                }
                let branches: Vec<&Expr> = whens
                    .iter()
                    .map(|(_, r)| r)
                    .chain(else_result.as_deref())
                    .collect();
                self.walk_siblings(&branches)?;
            }
            Expr::Coalesce(args) => {
                let refs: Vec<&Expr> = args.iter().collect();
                self.walk_siblings(&refs)?;
            }
            Expr::NullIf(a, b) => self.walk_siblings(&[a.as_ref(), b.as_ref()])?,
            Expr::Number(_)
            | Expr::String(_)
            | Expr::Ident(_)
            | Expr::Null
            | Expr::DateLiteral(_)
            | Expr::TimestampLiteral(_) => {}
        }
        Ok(())
    }

    /// 兄弟式の `$n` には、兄弟のうち型が決まるものの型を割り当てる。
    fn walk_siblings(&mut self, siblings: &[&Expr]) -> Result<(), SqlSurfaceError> {
        let sibling = self.sibling_type(siblings.iter().copied());
        for e in siblings {
            match as_param(e) {
                Some(n) => self.assign(n, sibling.clone())?,
                None => self.walk(e)?,
            }
        }
        Ok(())
    }
}

/// 式セグメント `tokens[seg.start..seg.end]` の各 `$n` 出現の型を推論する。
/// `root_type` は代入先の列の型（`None` は表・列がスキーマから引けない）で、セグメント全体が
/// 括弧付きの `$n` 単独の場合にだけ使う。`column_type` は式中の列参照の型を引く関数。
/// 戻り値は `(番号, 結果)` の出現順列。
pub(crate) fn infer_segment(
    tokens: &[Token],
    seg: &ExprSegment,
    for_set: bool,
    root_type: Option<PreparedParamType>,
    column_type: &dyn Fn(&str) -> Option<PreparedParamType>,
) -> Result<Vec<(u16, ParamInference)>, SqlSurfaceError> {
    let slice = tokens.get(seg.start..seg.end).ok_or_else(|| {
        SqlSurfaceError::unsupported("parameter placeholder is not supported at this position")
    })?;
    let replaced: Vec<Token> = slice
        .iter()
        .map(|t| match t {
            Token::Param(n) => Token::Ident(placeholder_name(*n)),
            other => other.clone(),
        })
        .collect();
    let expr = allowlist::parse_param_inference_expr(&replaced, for_set)?;
    let mut inferrer = Inferrer {
        column_type,
        out: Vec::new(),
    };
    if let Some(n) = as_param(&expr) {
        let ty = match root_type {
            Some(t) => Ty::Known(t),
            None => Ty::Unresolved,
        };
        inferrer.assign(n, ty)?;
    } else {
        inferrer.walk(&expr)?;
    }
    Ok(inferrer.out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize_with_params;
    use crate::sql::params::{expr_param_segments, validate_param_positions};

    fn int_col(name: &str) -> PreparedParamType {
        scalar(name, ColumnType::Integer)
    }

    fn lookup(name: &str) -> Option<PreparedParamType> {
        match name {
            "n" | "m" => Some(int_col(name)),
            "big" => Some(scalar(name, ColumnType::BigInt)),
            "x" => Some(scalar(name, ColumnType::Double)),
            "s" => Some(scalar(name, ColumnType::Text)),
            "b" => Some(scalar(name, ColumnType::Boolean)),
            _ => None,
        }
    }

    /// 文全体から各セグメントを推論し、出現順に並べて返す。
    fn infer(sql: &str) -> Result<Vec<(u16, ParamInference)>, SqlSurfaceError> {
        let tokens = tokenize_with_params(sql).expect("lex");
        validate_param_positions(&tokens)?;
        let mut all = Vec::new();
        for seg in expr_param_segments(&tokens) {
            let for_set = matches!(seg.target, crate::sql::params::ExprTarget::Set(_));
            let root = match &seg.target {
                crate::sql::params::ExprTarget::Set(col) => lookup(col),
                crate::sql::params::ExprTarget::Insert(_) => Some(int_col("t")),
            };
            all.extend(infer_segment(&tokens, &seg, for_set, root, &lookup)?);
        }
        Ok(all)
    }

    fn kind(r: &ParamInference) -> Option<ColumnType> {
        match r {
            ParamInference::Known(PreparedParamType::Column(ColumnMeta::Scalar { ty, .. })) => {
                Some(ty.clone())
            }
            _ => None,
        }
    }

    #[test]
    fn set_arithmetic_takes_the_column_type() {
        let r = infer("UPDATE t SET n = n + $1 WHERE id = $2").expect("infer");
        assert_eq!(r.len(), 1);
        assert_eq!(kind(&r[0].1), Some(ColumnType::Integer));
        let r = infer("UPDATE t SET x = $1 * x WHERE id = 1").expect("infer");
        assert_eq!(kind(&r[0].1), Some(ColumnType::Double));
    }

    #[test]
    fn literal_operand_types_follow_literal_shape() {
        let r = infer("UPDATE t SET n = $1 + 1 WHERE id = 1").expect("infer");
        assert_eq!(kind(&r[0].1), Some(ColumnType::Integer));
        let r = infer("UPDATE t SET n = $1 + 4294967296 WHERE id = 1").expect("infer");
        assert_eq!(kind(&r[0].1), Some(ColumnType::BigInt));
        let r = infer("UPDATE t SET x = $1 + 1.5 WHERE id = 1").expect("infer");
        assert_eq!(kind(&r[0].1), Some(ColumnType::Double));
    }

    #[test]
    fn parenthesised_lone_param_takes_the_target_column_type() {
        let r = infer("UPDATE t SET big = ($1) WHERE id = 1").expect("infer");
        assert_eq!(kind(&r[0].1), Some(ColumnType::BigInt));
    }

    #[test]
    fn builtin_text_argument_is_text_and_numeric_argument_is_indeterminate() {
        let r = infer("UPDATE t SET s = upper($1) WHERE id = 1").expect("infer");
        assert_eq!(r[0].1, ParamInference::Known(PreparedParamType::Text));
        let r = infer("UPDATE t SET n = abs($1) WHERE id = 1").expect("infer");
        assert_eq!(r[0].1, ParamInference::Indeterminate);
    }

    #[test]
    fn unknown_function_argument_falls_back_without_indeterminate() {
        let r = infer("UPDATE t SET n = my_udf($1) WHERE id = 1").expect("infer");
        assert_eq!(r[0].1, ParamInference::Unresolved);
    }

    #[test]
    fn unknown_column_is_unresolved_not_indeterminate() {
        let r = infer("UPDATE t SET zz = zz + $1 WHERE id = 1").expect("infer");
        assert_eq!(r[0].1, ParamInference::Unresolved);
    }

    #[test]
    fn two_params_without_a_typed_sibling_are_indeterminate() {
        let r = infer("UPDATE t SET n = $1 + $2 WHERE id = 1").expect("infer");
        assert!(r.iter().all(|(_, i)| *i == ParamInference::Indeterminate));
        let r = infer("UPDATE t SET n = COALESCE($1, $2) WHERE id = 1").expect("infer");
        assert!(r.iter().all(|(_, i)| *i == ParamInference::Indeterminate));
    }

    #[test]
    fn coalesce_and_case_use_typed_siblings() {
        let r = infer("UPDATE t SET n = COALESCE($1, n) WHERE id = 1").expect("infer");
        assert_eq!(kind(&r[0].1), Some(ColumnType::Integer));
        let r = infer("UPDATE t SET n = CASE WHEN m = $1 THEN $2 ELSE n END WHERE id = 1")
            .expect("infer");
        assert_eq!(r.len(), 2);
        assert_eq!(kind(&r[0].1), Some(ColumnType::Integer));
        assert_eq!(kind(&r[1].1), Some(ColumnType::Integer));
    }

    #[test]
    fn unsupported_type_inside_expression_is_feature_not_supported() {
        let e = infer("UPDATE t SET n = CASE WHEN b = $1 THEN 1 ELSE 0 END WHERE id = 1")
            .expect_err("boolean operand");
        assert_eq!(e.wire_code(), "0A000");
    }

    #[test]
    fn insert_values_expression_is_inferred_from_the_operand() {
        let r = infer("INSERT INTO t (n, s) VALUES (1 + $1, upper($2)) USING OPERATION_ID 'a'")
            .expect("infer");
        assert_eq!(kind(&r[0].1), Some(ColumnType::Integer));
        assert_eq!(r[1].1, ParamInference::Known(PreparedParamType::Text));
    }

    #[test]
    fn case_condition_equals_does_not_switch_the_assignment_target() {
        // 右辺内の `=` で代入先の列が切り替わらない（セグメントは `(SET|,) 列 =` だけで確定）。
        let tokens = tokenize_with_params(
            "UPDATE t SET n = CASE WHEN m = $1 THEN 1 ELSE 0 END, s = upper($2) WHERE id = 1",
        )
        .expect("lex");
        let segs = expr_param_segments(&tokens);
        assert_eq!(segs.len(), 2);
        assert_eq!(
            segs[0].target,
            crate::sql::params::ExprTarget::Set("n".to_string())
        );
        assert_eq!(
            segs[1].target,
            crate::sql::params::ExprTarget::Set("s".to_string())
        );
    }
}
