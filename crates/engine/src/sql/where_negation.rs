//! WHERE 述語ツリーの否定を構文段で葉まで押し下げる純関数群（SQL-24・TASK-208 の
//! ポインタ。Issue #1184）。
//!
//! 役割: `sql::allowlist::Parser` が `NOT ( ... )` や数値リテラルの `NOT IN`／
//! `NOT BETWEEN` を受理する際に呼ぶ。束縛段・実行経路（`sql::where_tree` の
//! 二値評価器を含む）は AST の形を変えずに済むよう、`WherePredicate` に新しい
//! variant を足さず既存の variant だけで否定を表現する。
//!
//! 設計の要点（fail-closed）: `sql::where_tree::BoundOrGroup::matches` 等の二値評価器は
//! UNKNOWN を false として扱うため、`Or`／AND 群の**上**に `Not` を置くと NULL 行で
//! UNKNOWN が真へ反転し fail-open になる。そこで De Morgan の法則（Kleene 三値論理でも
//! 厳密に成立する）で否定を葉まで押し下げ、葉の否定だけを既存の `WherePredicate::Not`
//! （宣言的フィルタの三値評価）または演算子の反転で表現する。
//!
//! 否定表:
//!
//! | 入力 | 結果 |
//! | ---- | ---- |
//! | 連言 `[p]` | `¬p` |
//! | 連言 `[p1..pn]`（n≥2） | `[Or([¬p1], ..., [¬pn])]` |
//! | `Or(branches)` | 各分岐の否定を連結（AND） |
//! | `Not(x)` | `x` |
//! | `IsNull` | `negated` を反転（IS NULL は UNKNOWN にならず厳密に同値） |
//! | 宣言的な葉 | `Not(leaf)` |
//! | `Expression(a > b)` 等 | 演算子反転（`>`↔`<=`、`<`↔`>=`） |
//! | `Expression(a = b)` | `Or([a < b], [a > b])`（`BinOp` に `<>` が無いため） |
//! | `visible()` | `42601`（RLS-7: 否定越しに RLS 述語を扱わせない） |
//! | `IN (SELECT)`／`EXISTS` | `Not(..)`（解決段 `sql::subquery` が NULL 規則込みで否定形を評価する。Issue #1191） |
//! | `<col> <op> (SELECT)` | 演算子反転（`=`↔`<>`・`<`↔`>=`・`>`↔`<=`。Issue #1191） |
//!
//! 演算子反転の根拠: 束縛段の比較は同型（数値×数値・TEXT×TEXT・DATE/TIMESTAMP 系）
//! でのみ成立し、値は全順序を持つ（非有限の REAL/DOUBLE は束縛・評価で拒否される）。
//! NULL が絡む場合は反転前後とも UNKNOWN になるので三値論理でも同値。

use crate::sql::allowlist::{SqlSurfaceError, WherePredicate};
use crate::sql::udf_call::{BinOp, Expr};

/// 式ノード数（パーサー予算 `consume_expr_node` と同じ単位）を数える。
/// 深さはパーサーが `MAX_EXPR_DEPTH` で有界にしているため再帰は有界。
pub(crate) fn expr_node_count(expr: &Expr) -> usize {
    match expr {
        Expr::Binary { lhs, rhs, .. } => 1usize
            .saturating_add(expr_node_count(lhs))
            .saturating_add(expr_node_count(rhs)),
        Expr::Call { args, .. } | Expr::Coalesce(args) => args
            .iter()
            .fold(1usize, |acc, a| acc.saturating_add(expr_node_count(a))),
        Expr::NullIf(a, b) => 1usize
            .saturating_add(expr_node_count(a))
            .saturating_add(expr_node_count(b)),
        Expr::Case { whens, else_result } => {
            let mut n = 1usize;
            for (c, r) in whens {
                n = n
                    .saturating_add(expr_node_count(c))
                    .saturating_add(expr_node_count(r));
            }
            if let Some(e) = else_result {
                n = n.saturating_add(expr_node_count(e));
            }
            n
        }
        Expr::Number(_)
        | Expr::String(_)
        | Expr::Ident(_)
        | Expr::Null
        | Expr::DateLiteral(_)
        | Expr::TimestampLiteral(_) => 1,
    }
}

/// 連言（AND で結ばれた述語列）の否定を、`Not` を群の上に残さない連言として返す。
/// `budget` はパーサーの式ノード予算（`Parser::expr_node_budget`）で、複製する
/// `Expr` のノード数を課金する（枯渇は `54000`）。
pub(crate) fn negate_conjunction(
    preds: Vec<WherePredicate>,
    budget: &mut usize,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    let mut iter = preds.into_iter();
    let Some(first) = iter.next() else {
        return Err(SqlSurfaceError::unsupported(
            "NOT must be followed by a predicate",
        ));
    };
    let Some(second) = iter.next() else {
        return negate_one(first, budget);
    };
    let mut branches = vec![negate_one(first, budget)?, negate_one(second, budget)?];
    for p in iter {
        branches.push(negate_one(p, budget)?);
    }
    Ok(vec![WherePredicate::Or(branches)])
}

fn charge(budget: &mut usize, n: usize) -> Result<(), SqlSurfaceError> {
    *budget = budget.checked_sub(n).ok_or_else(|| {
        SqlSurfaceError::payload_too_large("expression exceeds the allowed node count")
    })?;
    Ok(())
}

fn negate_one(
    pred: WherePredicate,
    budget: &mut usize,
) -> Result<Vec<WherePredicate>, SqlSurfaceError> {
    match pred {
        WherePredicate::Or(branches) => {
            let mut out = Vec::new();
            for branch in branches {
                out.extend(negate_conjunction(branch, budget)?);
            }
            Ok(out)
        }
        WherePredicate::Not(inner) => Ok(vec![*inner]),
        WherePredicate::IsNull { column, negated } => Ok(vec![WherePredicate::IsNull {
            column,
            negated: !negated,
        }]),
        WherePredicate::PredicateCall { .. } => Err(SqlSurfaceError::unsupported(
            "NOT visible() is not supported",
        )),
        // サブクエリの否定は解決段が評価する（Issue #1191）。`Not(Not(x))` は上の
        // `Not` 腕で畳まれる。
        leaf @ (WherePredicate::Exists { .. } | WherePredicate::InSubquery { .. }) => {
            Ok(vec![WherePredicate::Not(Box::new(leaf))])
        }
        WherePredicate::ScalarSubqueryCompare {
            column,
            op,
            inner_tokens,
            depth,
        } => Ok(vec![WherePredicate::ScalarSubqueryCompare {
            column,
            op: op.negated(),
            inner_tokens,
            depth,
        }]),
        WherePredicate::Expression(Expr::Binary { op, lhs, rhs }) => match op {
            BinOp::Gt | BinOp::Lt | BinOp::Ge | BinOp::Le => {
                let flipped = match op {
                    BinOp::Gt => BinOp::Le,
                    BinOp::Lt => BinOp::Ge,
                    BinOp::Ge => BinOp::Lt,
                    _ => BinOp::Gt,
                };
                Ok(vec![WherePredicate::Expression(Expr::Binary {
                    op: flipped,
                    lhs,
                    rhs,
                })])
            }
            BinOp::Eq => {
                charge(
                    budget,
                    expr_node_count(&lhs)
                        .saturating_add(expr_node_count(&rhs))
                        .saturating_add(2),
                )?;
                let lt = WherePredicate::Expression(Expr::Binary {
                    op: BinOp::Lt,
                    lhs: lhs.clone(),
                    rhs: rhs.clone(),
                });
                let gt = WherePredicate::Expression(Expr::Binary {
                    op: BinOp::Gt,
                    lhs,
                    rhs,
                });
                Ok(vec![WherePredicate::Or(vec![vec![lt], vec![gt]])])
            }
            _ => Err(SqlSurfaceError::unsupported(
                "NOT must be followed by a comparison",
            )),
        },
        WherePredicate::Expression(_) | WherePredicate::IdCompare { .. } => Err(
            SqlSurfaceError::unsupported("NOT must be followed by a comparison"),
        ),
        leaf @ (WherePredicate::Equality { .. }
        | WherePredicate::Prefix { .. }
        | WherePredicate::BoolEquality { .. }
        | WherePredicate::BoolColumn { .. }
        | WherePredicate::Compare { .. }
        | WherePredicate::InList { .. }
        | WherePredicate::Between { .. }) => Ok(vec![WherePredicate::Not(Box::new(leaf))]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(op: BinOp, l: &str, r: &str) -> WherePredicate {
        WherePredicate::Expression(Expr::Binary {
            op,
            lhs: Box::new(Expr::Ident(l.to_string())),
            rhs: Box::new(Expr::Number(r.to_string())),
        })
    }

    #[test]
    fn flips_ordering_ops_and_is_null() {
        let mut b = 100;
        assert_eq!(
            negate_conjunction(vec![cmp(BinOp::Gt, "id", "1")], &mut b).unwrap(),
            vec![cmp(BinOp::Le, "id", "1")]
        );
        assert_eq!(
            negate_conjunction(
                vec![WherePredicate::IsNull {
                    column: "a".into(),
                    negated: false
                }],
                &mut b
            )
            .unwrap(),
            vec![WherePredicate::IsNull {
                column: "a".into(),
                negated: true
            }]
        );
    }

    #[test]
    fn eq_becomes_lt_or_gt_and_charges_budget() {
        let mut b = 100;
        let out = negate_conjunction(vec![cmp(BinOp::Eq, "id", "1")], &mut b).unwrap();
        assert_eq!(
            out,
            vec![WherePredicate::Or(vec![
                vec![cmp(BinOp::Lt, "id", "1")],
                vec![cmp(BinOp::Gt, "id", "1")]
            ])]
        );
        assert!(b < 100);
        let mut zero = 0;
        assert!(matches!(
            negate_conjunction(vec![cmp(BinOp::Eq, "id", "1")], &mut zero),
            Err(SqlSurfaceError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn de_morgan_on_and_and_or() {
        let mut b = 100;
        let a = WherePredicate::Equality {
            column: "x".into(),
            value: "1".into(),
        };
        let c = WherePredicate::Equality {
            column: "y".into(),
            value: "2".into(),
        };
        let not = |p: &WherePredicate| WherePredicate::Not(Box::new(p.clone()));
        // NOT (a AND c) = Or([NOT a], [NOT c])
        assert_eq!(
            negate_conjunction(vec![a.clone(), c.clone()], &mut b).unwrap(),
            vec![WherePredicate::Or(vec![vec![not(&a)], vec![not(&c)]])]
        );
        // NOT (a OR c) = [NOT a, NOT c]
        assert_eq!(
            negate_conjunction(
                vec![WherePredicate::Or(vec![vec![a.clone()], vec![c.clone()]])],
                &mut b
            )
            .unwrap(),
            vec![not(&a), not(&c)]
        );
        // 二重否定は畳む。
        assert_eq!(
            negate_conjunction(vec![not(&a)], &mut b).unwrap(),
            vec![a.clone()]
        );
    }

    #[test]
    fn rejects_visible_and_wraps_subqueries() {
        let mut b = 100;
        assert!(matches!(
            negate_conjunction(
                vec![WherePredicate::PredicateCall {
                    name: "visible".into()
                }],
                &mut b
            ),
            Err(SqlSurfaceError::UnsupportedSyntax { .. })
        ));
        let exists = WherePredicate::Exists {
            inner_tokens: vec![],
            depth: 1,
        };
        assert_eq!(
            negate_conjunction(vec![exists.clone()], &mut b).unwrap(),
            vec![WherePredicate::Not(Box::new(exists.clone()))]
        );
        // 二重否定は畳まれる。
        assert_eq!(
            negate_conjunction(vec![WherePredicate::Not(Box::new(exists.clone()))], &mut b)
                .unwrap(),
            vec![exists]
        );
    }

    #[test]
    fn scalar_subquery_negation_flips_operator() {
        use crate::sql::allowlist::ScalarSubqueryOp;
        let mut b = 100;
        let p = |op| WherePredicate::ScalarSubqueryCompare {
            column: "c".into(),
            op,
            inner_tokens: vec![],
            depth: 1,
        };
        for (a, n) in [
            (ScalarSubqueryOp::Eq, ScalarSubqueryOp::Ne),
            (ScalarSubqueryOp::Lt, ScalarSubqueryOp::Ge),
            (ScalarSubqueryOp::Gt, ScalarSubqueryOp::Le),
        ] {
            assert_eq!(negate_conjunction(vec![p(a)], &mut b).unwrap(), vec![p(n)]);
            assert_eq!(negate_conjunction(vec![p(n)], &mut b).unwrap(), vec![p(a)]);
        }
    }
}
