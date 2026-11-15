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

fn leaf(op: BinOp, l: &Expr, r: &Expr) -> WherePredicate {
    WherePredicate::Expression(Expr::Binary {
        op,
        lhs: Box::new(l.clone()),
        rhs: Box::new(r.clone()),
    })
}

type Pair<'a> = (&'a Expr, &'a Expr);

fn eq_conjunction(pairs: &[Pair<'_>]) -> Vec<WherePredicate> {
    pairs.iter().map(|(l, r)| leaf(BinOp::Eq, l, r)).collect()
}

/// 辞書式比較の展開。`pairs` は 1 個以上（空は `Internal`）。
fn lex(
    pairs: &[Pair<'_>],
    strict: BinOp,
    last: BinOp,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    match pairs {
        [] => Err(internal("empty row comparison")),
        [(l, r)] => Ok(vec![leaf(last, l, r)]),
        _ => {
            let (a, b) = pairs.split_at(pairs.len() / 2);
            let mut branches: Vec<Vec<WherePredicate>> = Vec::new();
            let mut strict_a = lex(a, strict, strict)?;
            // A 側が単一の `Or` なら分岐を親へ平坦化する（入れ子の深さを増やさない）。
            if matches!(strict_a.as_slice(), [WherePredicate::Or(_)]) {
                if let Some(WherePredicate::Or(inner)) = strict_a.pop() {
                    branches.extend(inner);
                }
            } else {
                branches.push(strict_a);
            }
            let mut tail = eq_conjunction(a);
            tail.extend(lex(b, strict, last)?);
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
    match op {
        ArrayCmpOp::Eq => Ok(eq_conjunction(&pairs)),
        ArrayCmpOp::Ne => {
            crate::sql::where_negation::negate_conjunction(eq_conjunction(&pairs), budget)
        }
        ArrayCmpOp::Lt => lex(&pairs, BinOp::Lt, BinOp::Lt),
        ArrayCmpOp::Le => lex(&pairs, BinOp::Lt, BinOp::Le),
        ArrayCmpOp::Gt => lex(&pairs, BinOp::Gt, BinOp::Gt),
        ArrayCmpOp::Ge => lex(&pairs, BinOp::Gt, BinOp::Ge),
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
}
