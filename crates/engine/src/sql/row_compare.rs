//! WHERE の行値比較 `(e1, ..., en) <op> (f1, ..., fn)` の評価（対象ビヘイビア: SQL-9。
//! 親 Issue #3254・本 Issue #3256）。
//!
//! 役割と責務境界:
//! - 構文段（`sql::allowlist`）が生成した [`WherePredicate::RowCompare`] を、束縛段
//!   （`sql::parser::bind_where_predicates_recursive`）の入口で既存の述語木
//!   （`Expression(Expr::Binary)` の葉・`Or`・連言）へ展開する。以降は通常の比較と同じ束縛・評価経路を
//!   通るため、第 2 の評価器は作らず、成分の型照合・NUMERIC の宣言的レーンへの書き換えも通常の比較と
//!   構成上一致する（`sql::where_tree` の契約を維持）。
//! - 文字列の組み立ても再解析もしない（AST の複製のみ）。
//!
//! 展開規則（Kleene 三値論理で厳密に同値。`s` は厳密側の演算子、`t` は元の演算子）:
//! - `=`: 成分ごとの `=` の連言。
//! - `<>`: 上の連言の否定（`where_negation::negate_conjunction`。成分ごとの `<>` の `OR`）。
//! - 辞書式（`<`・`<=`・`>`・`>=`）: 成分列を前半 A・後半 B に二分し
//!   `R_t(AB) = R_s(A) OR (EQ(A) AND R_t(B))` を再帰する。A に決定成分があれば、非 NULL なら `EQ(A)` が
//!   FALSE で `R_s(A)` がそのまま結果になり、NULL なら `R_s(A)` も右辺も FALSE か NULL なので結果は NULL。
//!   A が全部等しければ `R_s(A)` は FALSE・`EQ(A)` は TRUE で `R_t(B)` になる。
//!   均衡分割のため `Or` の入れ子は高々 ⌈log2 n⌉、比較数は n + (n/2)·log2 n で、既存の
//!   式ノード予算（`54000`）と群の深さ上限の内側に収まる。

use crate::sql::allowlist::{SqlSurfaceError, WherePredicate};
use crate::sql::udf_call::{ArrayCmpOp, BinOp, Expr};

/// 述語木（`Or` 分岐・`Not` の内側を含む）に行値比較が残っているか。評価未対応の文脈
/// （ポリシー式など）が、保存・描画の前に fail-closed で拒否するために使う。
pub(crate) fn contains_row_compare(preds: &[WherePredicate]) -> bool {
    preds.iter().any(|p| match p {
        WherePredicate::RowCompare { .. } => true,
        WherePredicate::Or(branches) => branches.iter().any(|b| contains_row_compare(b)),
        WherePredicate::Not(inner) => contains_row_compare(std::slice::from_ref(inner.as_ref())),
        _ => false,
    })
}

/// トップレベルの連言に含まれる行値比較を展開した連言を返す（行値比較以外は複製）。
/// `Or` 分岐の中の行値比較は、分岐ごとの再帰束縛でこの関数が再び適用される。
/// `Not(RowCompare)` は構文段が演算子反転へ押し下げるため届かない想定で、届いたら `Internal`。
pub(crate) fn lower_top_level(
    preds: &[WherePredicate],
    budget: &mut usize,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    let mut out = Vec::with_capacity(preds.len());
    for p in preds {
        match p {
            WherePredicate::RowCompare { lhs, op, rhs } => {
                out.extend(lower_row_compare(lhs, *op, rhs, budget)?);
            }
            WherePredicate::Not(inner)
                if matches!(inner.as_ref(), WherePredicate::RowCompare { .. }) =>
            {
                return Err(internal("negated row comparison reached binding"));
            }
            other => out.push(other.clone()),
        }
    }
    Ok(out)
}

fn internal(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: detail.to_string(),
    }
}

/// 展開で複製する式ノード数を `remaining`（展開前の式ノード予算の写し）から引く（枯渇は `54000`）。
/// 辞書式展開は成分式を複数の比較葉へ複製するため、全経路（`=`・`<>`・辞書式）で葉の生成ごとに数え、
/// 非自明な成分を並べても展開後の木が予算の外へ膨らむ前に（束縛段へ渡す前に）一貫して拒否する。
/// 実際の課金は束縛段が展開後の木に対して行うため、ここでは本物の予算を消費しない（二重課金の回避）。
fn charge(budget: &mut usize, n: usize) -> Result<(), SqlSurfaceError> {
    *budget = budget.checked_sub(n).ok_or_else(|| {
        SqlSurfaceError::payload_too_large("expression exceeds the allowed node count")
    })?;
    Ok(())
}

/// 真偽値リテラルの `true` / `false`（構文段は行値の成分をそのまま `Expr::Ident` で保持する）。
fn bool_literal(e: &Expr) -> Option<bool> {
    match e {
        Expr::Ident(w) if w.eq_ignore_ascii_case("true") => Some(true),
        Expr::Ident(w) if w.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

/// 比較葉 1 件を作る。`<col> = TRUE|FALSE`（どちらの向きでも）は通常の比較と同じく
/// 宣言的な `BoolEquality` にする（BOOLEAN 列は式内で束縛できないため）。
/// それ以外の BOOLEAN 成分・INTERVAL 成分は通常の比較と同じ束縛エラー（fail-closed）になる。
fn leaf(
    op: BinOp,
    l: &Expr,
    r: &Expr,
    budget: &mut usize,
) -> Result<WherePredicate, SqlSurfaceError> {
    if op == BinOp::Eq {
        let col_and_value = match (l, r) {
            (Expr::Ident(c), v) if bool_literal(l).is_none() => bool_literal(v).map(|b| (c, b)),
            (v, Expr::Ident(c)) if bool_literal(r).is_none() => bool_literal(v).map(|b| (c, b)),
            _ => None,
        };
        if let Some((column, value)) = col_and_value {
            charge(budget, 1)?;
            return Ok(WherePredicate::BoolEquality {
                column: column.clone(),
                value,
            });
        }
    }
    charge(
        budget,
        crate::sql::where_negation::expr_node_count(l)
            .saturating_add(crate::sql::where_negation::expr_node_count(r))
            .saturating_add(1),
    )?;
    Ok(WherePredicate::Expression(Expr::Binary {
        op,
        lhs: Box::new(l.clone()),
        rhs: Box::new(r.clone()),
    }))
}

type Pair<'a> = (&'a Expr, &'a Expr);

fn eq_conjunction(
    pairs: &[Pair<'_>],
    budget: &mut usize,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    pairs
        .iter()
        .map(|(l, r)| leaf(BinOp::Eq, l, r, budget))
        .collect()
}

/// 辞書式比較の展開。`pairs` は 1 個以上（空は `Internal`）。
fn lex(
    pairs: &[Pair<'_>],
    strict: BinOp,
    last: BinOp,
    budget: &mut usize,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    match pairs {
        [] => Err(internal("empty row comparison")),
        [(l, r)] => Ok(vec![leaf(last, l, r, budget)?]),
        _ => {
            let (a, b) = pairs.split_at(pairs.len() / 2);
            let mut branches: Vec<Vec<WherePredicate>> = Vec::new();
            let mut strict_a = lex(a, strict, strict, budget)?;
            // A 側が単一の `Or` なら分岐を親へ平坦化する（入れ子の深さを増やさない）。
            if matches!(strict_a.as_slice(), [WherePredicate::Or(_)]) {
                if let Some(WherePredicate::Or(inner)) = strict_a.pop() {
                    branches.extend(inner);
                }
            } else {
                branches.push(strict_a);
            }
            let mut tail = eq_conjunction(a, budget)?;
            tail.extend(lex(b, strict, last, budget)?);
            branches.push(tail);
            Ok(vec![WherePredicate::Or(branches)])
        }
    }
}

/// 行値比較 1 件を、既存の述語だけで構成した連言へ展開する。
pub(crate) fn lower_row_compare(
    lhs: &[Expr],
    op: ArrayCmpOp,
    rhs: &[Expr],
    budget: &mut usize,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    if lhs.len() != rhs.len()
        || lhs.len() < 2
        || lhs.len() > crate::sql::allowlist::MAX_ROW_COMPARE_COMPONENTS
    {
        return Err(internal(
            "row comparison component count invariant violated",
        ));
    }
    let pairs: Vec<Pair<'_>> = lhs.iter().zip(rhs.iter()).collect();
    let mut remaining = *budget;
    match op {
        ArrayCmpOp::Eq => eq_conjunction(&pairs, &mut remaining),
        ArrayCmpOp::Ne => {
            let eqs = eq_conjunction(&pairs, &mut remaining)?;
            // 否定の課金も写しで試算する（生きた予算へ課金すると束縛段の走査と二重課金になる）。
            let mut negation_budget = *budget;
            crate::sql::where_negation::negate_conjunction(eqs, &mut negation_budget)
        }
        ArrayCmpOp::Lt => lex(&pairs, BinOp::Lt, BinOp::Lt, &mut remaining),
        ArrayCmpOp::Le => lex(&pairs, BinOp::Lt, BinOp::Le, &mut remaining),
        ArrayCmpOp::Gt => lex(&pairs, BinOp::Gt, BinOp::Gt, &mut remaining),
        ArrayCmpOp::Ge => lex(&pairs, BinOp::Gt, BinOp::Ge, &mut remaining),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exprs(n: usize) -> Vec<Expr> {
        (0..n).map(|i| Expr::Ident(format!("c{i}"))).collect()
    }

    fn depth(preds: &[WherePredicate]) -> usize {
        preds
            .iter()
            .map(|p| match p {
                WherePredicate::Or(bs) => 1 + bs.iter().map(|b| depth(b)).max().unwrap_or(0),
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn lexicographic_expansion_is_shallow_for_max_components() {
        let n = crate::sql::allowlist::MAX_ROW_COMPARE_COMPONENTS;
        let mut budget = usize::MAX;
        let out = lower_row_compare(&exprs(n), ArrayCmpOp::Lt, &exprs(n), &mut budget).unwrap();
        assert!(depth(&out) <= 7, "depth {}", depth(&out));
    }

    #[test]
    fn two_components_expand_to_two_branches() {
        let mut budget = usize::MAX;
        let out = lower_row_compare(&exprs(2), ArrayCmpOp::Le, &exprs(2), &mut budget).unwrap();
        let [WherePredicate::Or(branches)] = out.as_slice() else {
            panic!("expected a single Or");
        };
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0].len(), 1);
        assert_eq!(branches[1].len(), 2);
    }

    #[test]
    fn component_count_invariant_is_checked() {
        let mut budget = usize::MAX;
        assert!(lower_row_compare(&exprs(1), ArrayCmpOp::Eq, &exprs(1), &mut budget).is_err());
        assert!(lower_row_compare(&exprs(2), ArrayCmpOp::Eq, &exprs(3), &mut budget).is_err());
    }

    #[test]
    fn expansion_does_not_consume_live_budget() {
        // 展開は予算の写しで試算するのみで、生きた予算は束縛段の課金のために手付かずで残す。
        for op in [
            ArrayCmpOp::Eq,
            ArrayCmpOp::Ne,
            ArrayCmpOp::Lt,
            ArrayCmpOp::Le,
            ArrayCmpOp::Gt,
            ArrayCmpOp::Ge,
        ] {
            let mut budget = 1000usize;
            lower_row_compare(&exprs(3), op, &exprs(3), &mut budget).unwrap();
            assert_eq!(budget, 1000, "live budget consumed for {op:?}");
        }
    }
}
