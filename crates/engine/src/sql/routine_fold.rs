//! `LANGUAGE sql|plpgsql` 関数の呼び出し位置の畳み込み（Issue #2218・SQL-67 (5)。設計は
//! `docs/design/plpgsql-subset.md` の「Issue #2218 追記」）。
//!
//! 役割: `FROM` の無い `SELECT`（`sql::const_select`）の投影項目・`WHERE` の式木から、セッションの
//! [`UdfRegistry`] に登録されたルーチンの呼び出しを探し、引数を既存の束縛器・評価器で評価して
//! [`RoutineInvoker`]（core の `core::routine_call`）へ渡し、戻り値を型付きリテラルの [`Expr`] に
//! 置き換える。置き換え後の式木にはルーチンの呼び出しが残らないため、以降の束縛・評価は既存の経路を
//! そのまま使う（`BoundExpr` に variant を足さず、第 2 の式評価器も作らない）。
//!
//! 評価順は PostgreSQL に合わせて引数を先に・左から右へ。評価が条件付きになる位置（`CASE` の分岐、
//! `COALESCE` の第 2 引数以降）に呼び出しがあれば、PostgreSQL が評価しない位置で揮発性の副作用や
//! エラーを起こさないよう `0A000` で拒否する。[`FoldMode::Describe`] は束縛（型検査）だけを行い、
//! 本体を実行せず、戻り値型の型付き NULL に置き換える（拡張クエリの Describe は本体を実行しない契約）。
//!
//! エラー文言は固定の英語で、関数名・本体・引数の値を含めない。

use std::sync::Arc;

use crate::plpgsql::host::PlScalar;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::cast::{self, CastRule, CastType};
use crate::sql::const_select::expr_value_to_cell;
use crate::sql::exec::Cell;
use crate::sql::udf_call::{
    bind_const_projection_expr, eval, Expr, RoutineDefinition, UdfRegistry, MAX_EXPR_NODES,
};

/// ルーチン 1 回の起動を担う側（core が実装する。`sql` モジュールはストレージ・Tx に触れない）。
/// `args` は宣言された引数型へ変換済みで、戻り値は戻り値型へ代入変換済み。
pub(crate) trait RoutineInvoker {
    fn invoke(
        &mut self,
        def: &RoutineDefinition,
        args: Vec<PlScalar>,
    ) -> Result<PlScalar, SqlSurfaceError>;
}

/// 畳み込みの動作。
pub(crate) enum FoldMode<'a> {
    /// ルーチンの呼び出しがあるかを調べるだけ（式木は変えない）。
    Detect,
    /// 束縛だけを行い、呼び出しを戻り値型の型付き NULL にする（本体は実行しない）。
    Describe,
    /// 引数を評価して起動し、結果を型付きリテラルにする。
    Run(&'a mut dyn RoutineInvoker),
}

/// 畳み込み 1 回分の状態。
pub(crate) struct Folder<'a, 'b> {
    registry: &'a UdfRegistry,
    mode: FoldMode<'b>,
    /// 偽の間は `Run` でも起動せず型付き NULL にする（`WHERE` が偽で出力行が無い投影項目）。
    active: bool,
    /// `Detect` で呼び出しを見つけたか。
    found: bool,
}

fn unsupported(detail: &'static str) -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(detail)
}

fn typed_null(ret: &CastType) -> Expr {
    Expr::Cast {
        expr: Box::new(Expr::Null),
        ty: ret.clone(),
    }
}

impl<'a, 'b> Folder<'a, 'b> {
    pub(crate) fn new(registry: &'a UdfRegistry, mode: FoldMode<'b>) -> Self {
        Self {
            registry,
            mode,
            active: true,
            found: false,
        }
    }

    /// 以降の `Run` の起動を許すか（`WHERE` が偽なら偽にする）。
    pub(crate) fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// 起動する動作（`Run`）か。
    pub(crate) fn runs(&self) -> bool {
        matches!(self.mode, FoldMode::Run(_))
    }

    /// `Detect` でルーチンの呼び出しを見つけたか。
    pub(crate) fn found(&self) -> bool {
        self.found
    }

    /// 式木 `e` を畳み込んだ式を返す。`conditional` は評価が条件付きになる位置か。
    pub(crate) fn fold(&mut self, e: &Expr, conditional: bool) -> Result<Expr, SqlSurfaceError> {
        match e {
            Expr::Number(_)
            | Expr::String(_)
            | Expr::Ident(_)
            | Expr::Null
            | Expr::DateLiteral(_)
            | Expr::TimestampLiteral(_)
            | Expr::TimeLiteral(_)
            | Expr::TimestampTzLiteral { .. }
            | Expr::IntervalLiteral(_)
            | Expr::ValueFunction { .. } => Ok(e.clone()),
            Expr::Call { name, args } => {
                let folded = args
                    .iter()
                    .map(|a| self.fold(a, conditional))
                    .collect::<Result<Vec<_>, _>>()?;
                match self.registry.get_routine(name).cloned() {
                    Some(def) => self.call(&def, folded, conditional, e),
                    None => Ok(Expr::Call {
                        name: name.clone(),
                        args: folded,
                    }),
                }
            }
            Expr::Binary { op, lhs, rhs } => Ok(Expr::Binary {
                op: *op,
                lhs: Box::new(self.fold(lhs, conditional)?),
                rhs: Box::new(self.fold(rhs, conditional)?),
            }),
            Expr::Case { whens, else_result } => {
                let mut out = Vec::with_capacity(whens.len());
                for (i, (c, r)) in whens.iter().enumerate() {
                    // 最初の条件だけは必ず評価される。それ以降の条件・結果・ELSE は条件付き。
                    let c = self.fold(c, conditional || i > 0)?;
                    let r = self.fold(r, true)?;
                    out.push((c, r));
                }
                let else_result = else_result
                    .as_deref()
                    .map(|x| self.fold(x, true).map(Box::new))
                    .transpose()?;
                Ok(Expr::Case {
                    whens: out,
                    else_result,
                })
            }
            Expr::Coalesce(args) => Ok(Expr::Coalesce(
                args.iter()
                    .enumerate()
                    .map(|(i, a)| self.fold(a, conditional || i > 0))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            Expr::NullIf(l, r) => Ok(Expr::NullIf(
                Box::new(self.fold(l, conditional)?),
                Box::new(self.fold(r, conditional)?),
            )),
            Expr::IsDistinctFrom { negated, lhs, rhs } => Ok(Expr::IsDistinctFrom {
                negated: *negated,
                lhs: Box::new(self.fold(lhs, conditional)?),
                rhs: Box::new(self.fold(rhs, conditional)?),
            }),
            Expr::RegexMatch {
                negated,
                case_insensitive,
                lhs,
                pattern,
                raw_pattern_len,
            } => Ok(Expr::RegexMatch {
                negated: *negated,
                case_insensitive: *case_insensitive,
                lhs: Box::new(self.fold(lhs, conditional)?),
                pattern: Box::new(self.fold(pattern, conditional)?),
                raw_pattern_len: *raw_pattern_len,
            }),
            Expr::JsonAccess { op, lhs, rhs } => Ok(Expr::JsonAccess {
                op: *op,
                lhs: Box::new(self.fold(lhs, conditional)?),
                rhs: Box::new(self.fold(rhs, conditional)?),
            }),
            Expr::Collate { expr, collation } => Ok(Expr::Collate {
                expr: Box::new(self.fold(expr, conditional)?),
                collation: *collation,
            }),
            Expr::Cast { expr, ty } => Ok(Expr::Cast {
                expr: Box::new(self.fold(expr, conditional)?),
                ty: ty.clone(),
            }),
            Expr::ArrayConstructor(_) | Expr::Subscript { .. } | Expr::Quantified { .. } => e
                .try_map_array_children(&mut |c| self.fold(c, conditional))
                .unwrap_or_else(|| Ok(e.clone())),
            Expr::SqlJsonQuery(q) => Ok(Expr::SqlJsonQuery(Box::new(
                q.try_map_children(&mut |c| self.fold(c, true))?,
            ))),
        }
    }

    /// 畳み込み済みの引数 `args` でルーチンを 1 回呼ぶ。`original` は `Detect` が式を変えずに返す用。
    fn call(
        &mut self,
        def: &Arc<RoutineDefinition>,
        args: Vec<Expr>,
        conditional: bool,
        original: &Expr,
    ) -> Result<Expr, SqlSurfaceError> {
        if matches!(self.mode, FoldMode::Detect) {
            self.found = true;
            return Ok(original.clone());
        }
        let Some(ret) = def.ret.clone() else {
            return Err(unsupported(
                "trigger functions cannot be called as ordinary functions",
            ));
        };
        if conditional {
            return Err(unsupported(
                "calling a LANGUAGE sql or plpgsql function in a conditionally evaluated expression is not supported",
            ));
        }
        if matches!(ret, CastType::Array(_)) {
            return Err(unsupported("unsupported return type of a function call"));
        }
        if args.len() != def.param_types.len() {
            return Err(SqlSurfaceError::undefined_function(
                "function does not exist or its argument count does not match",
            ));
        }
        // 引数の型検査と評価。`Describe`・起動しない場合も型検査は行い、どの経路でも同じエラーにする。
        let mut values: Vec<PlScalar> = Vec::with_capacity(args.len());
        let run = self.active && matches!(self.mode, FoldMode::Run(_));
        for (arg, declared) in args.iter().zip(def.param_types.iter()) {
            check_arg(self.registry, arg, declared)?;
            if run {
                values.push(eval_arg(self.registry, arg, declared)?);
            }
        }
        if !run {
            return Ok(typed_null(&ret));
        }
        // STRICT は引数のいずれかが NULL なら本体を実行せず NULL を返す。
        if def.attrs.strict && values.iter().any(PlScalar::is_null) {
            return Ok(typed_null(&ret));
        }
        let FoldMode::Run(invoker) = &mut self.mode else {
            return Err(SqlSurfaceError::Internal {
                detail: "routine fold mode mismatch".to_string(),
            });
        };
        let out = invoker.invoke(def, values)?;
        scalar_to_expr(out, &ret)
    }
}

/// 実引数 1 個の型検査（宣言的 UDF の型付き引数 `bind_typed_udf_arg` と同じ規則。型の決まった実引数は
/// 暗黙変換だけで受理し、型なしリテラル〔文字列・NULL〕は宣言型の入力関数で解釈する）。
fn check_arg(
    registry: &UdfRegistry,
    arg: &Expr,
    declared: &CastType,
) -> Result<(), SqlSurfaceError> {
    let mismatch = || {
        SqlSurfaceError::undefined_function("function argument type cannot be implicitly converted")
    };
    let implicit_ok = |rule: CastRule| matches!(rule, CastRule::Identity | CastRule::Implicit);
    match arg {
        Expr::String(_) | Expr::Null => Ok(()),
        Expr::Number(raw) => {
            let from = cast::number_literal_kind(raw);
            if implicit_ok(cast::cast_rule(from, declared.kind())) {
                Ok(())
            } else {
                Err(mismatch())
            }
        }
        _ => {
            let mut budget = MAX_EXPR_NODES;
            // 畳み込み済みの関数呼び出しは `CAST` の形になるため、投影と同じ束縛（`CAST` の値を許す）を使う。
            let (_bound, _ty, result_ty) = bind_const_projection_expr(arg, registry, &mut budget)?;
            let from = result_ty
                .as_ref()
                .and_then(cast::cast_type_of_column)
                .ok_or_else(|| unsupported("unsupported argument type of a function call"))?;
            let ok = match (&from, declared) {
                (CastType::Array(a), CastType::Array(b)) => {
                    implicit_ok(cast::cast_rule_types(a, b))
                }
                _ => implicit_ok(cast::cast_rule_types(&from, declared)),
            };
            if ok {
                Ok(())
            } else {
                Err(mismatch())
            }
        }
    }
}

/// 型検査済みの実引数を宣言型へ変換して評価する。
fn eval_arg(
    registry: &UdfRegistry,
    arg: &Expr,
    declared: &CastType,
) -> Result<PlScalar, SqlSurfaceError> {
    let wrapped = Expr::Cast {
        expr: Box::new(arg.clone()),
        ty: declared.clone(),
    };
    let mut budget = MAX_EXPR_NODES;
    let (bound, _ty, result_ty) = bind_const_projection_expr(&wrapped, registry, &mut budget)?;
    let ty = result_ty
        .clone()
        .ok_or_else(|| unsupported("unsupported argument type of a function call"))?;
    let cell = expr_value_to_cell(eval(&bound, 0, &[])?, Some(&ty))?;
    Ok(PlScalar::new(cell, ty))
}

/// 起動結果を、戻り値型 `ret` の型付きリテラル式へ。日時・間隔は専用のリテラル、それ以外は文字列表現を
/// `ret` へ `CAST` する（入力関数が値を厳密に復元する）。配列・ベクトル等は `0A000`。
fn scalar_to_expr(v: PlScalar, ret: &CastType) -> Result<Expr, SqlSurfaceError> {
    let text = match &v.cell {
        Cell::Null => return Ok(typed_null(ret)),
        Cell::Date(d) => return Ok(Expr::DateLiteral(*d)),
        Cell::Timestamp(t) => return Ok(Expr::TimestampLiteral(*t)),
        Cell::Time(t) => return Ok(Expr::TimeLiteral(*t)),
        Cell::TimestampTz(t) => {
            return Ok(Expr::TimestampTzLiteral {
                micros: *t,
                session_local: false,
            })
        }
        Cell::Interval(i) => return Ok(Expr::IntervalLiteral(*i)),
        Cell::SignedInteger(i) => i.to_string(),
        Cell::Integer(u) => u.to_string(),
        Cell::Float(f) => {
            if f.is_nan() {
                "NaN".to_string()
            } else if f.is_infinite() {
                if *f > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
            } else {
                format!("{f:?}")
            }
        }
        Cell::Bool(b) => b.to_string(),
        Cell::Text(s) | Cell::Json(s) => s.clone(),
        Cell::Numeric(d) => d.to_string(),
        Cell::Uuid(u) => u.to_string(),
        Cell::Bytes(b) => {
            let mut s = String::with_capacity(b.len().saturating_mul(2).saturating_add(2));
            s.push_str("\\x");
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            s
        }
        _ => return Err(unsupported("unsupported return type of a function call")),
    };
    Ok(Expr::Cast {
        expr: Box::new(Expr::String(text)),
        ty: ret.clone(),
    })
}
