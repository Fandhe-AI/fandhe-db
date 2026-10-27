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
//! | 空の連言（恒真。`TRUE` 畳み込み） | `[Or([])]`（恒偽。逆に `Or([])` の否定は空の連言） |
//! | 連言 `[p]` | `¬p` |
//! | 連言 `[p1..pn]`（n≥2） | `[Or([¬p1], ..., [¬pn])]` |
//! | `Or(branches)` | 各分岐の否定を連結（AND） |
//! | `Not(x)` | `x` |
//! | `IsNull` | `negated` を反転（IS NULL は UNKNOWN にならず厳密に同値） |
//! | 宣言的な葉 | `Not(leaf)` |
//! | `Expression(a > b)` 等 | 演算子反転（`>`↔`<=`、`<`↔`>=`） |
//! | `Expression(a = b)` | `Or([a < b], [a > b])`（`BinOp` に `<>` が無いため。`a <> b` もこの形へ脱糖） |
//! | `Expression(x op ANY\|ALL (a))` | 量化子を反転し演算子を否定（TABLE-35・Issue #1603） |
//! | `Expression(a IS [NOT] DISTINCT FROM b)` | `negated` を反転（結果が NULL にならず厳密に同値。SQL-54・Issue #1783） |
//! | `Expression(a ~ b)` 等の正規表現マッチ | `negated` を反転（左辺・パターンが NULL なら否定の有無に関わらず NULL のため三値論理でも同値。SQL-55・Issue #1787） |
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
        Expr::Binary { lhs, rhs, .. } | Expr::JsonAccess { lhs, rhs, .. } => 1usize
            .saturating_add(expr_node_count(lhs))
            .saturating_add(expr_node_count(rhs)),
        Expr::Call { args, .. } | Expr::Coalesce(args) => args
            .iter()
            .fold(1usize, |acc, a| acc.saturating_add(expr_node_count(a))),
        Expr::NullIf(a, b)
        | Expr::IsDistinctFrom { lhs: a, rhs: b, .. }
        | Expr::RegexMatch {
            lhs: a, pattern: b, ..
        } => 1usize
            .saturating_add(expr_node_count(a))
            .saturating_add(expr_node_count(b)),
        Expr::Collate { expr, .. } | Expr::Cast { expr, .. } => {
            1usize.saturating_add(expr_node_count(expr))
        }
        Expr::ArrayConstructor(_) | Expr::Subscript { .. } | Expr::Quantified { .. } => expr
            .array_expr_children()
            .into_iter()
            .flatten()
            .fold(1usize, |acc, c| acc.saturating_add(expr_node_count(c))),
        Expr::SqlJsonQuery(q) => q
            .children()
            .into_iter()
            .fold(1usize, |acc, c| acc.saturating_add(expr_node_count(c))),
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
        | Expr::TimestampLiteral(_)
        | Expr::IntervalLiteral(_)
        | Expr::TimeLiteral(_)
        | Expr::TimestampTzLiteral { .. }
        | Expr::ValueFunction { .. } => 1,
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
    // 空の連言は恒真（`WHERE TRUE` の畳み込み結果。SQL-41・Issue #1526）。定数は
    // NULL にならないため、その否定は恒偽（分岐 0 個の `Or`）で三値論理でも厳密。
    let Some(first) = iter.next() else {
        return Ok(vec![WherePredicate::Or(Vec::new())]);
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
        // 量化比較のサブクエリ形（Issue #1855・SQL-61 (3)）も同じ規則で、演算子と量化子の
        // 両方を反転する（De Morgan は三値論理でも厳密）。
        WherePredicate::QuantifiedSubqueryCompare {
            column,
            op,
            quantifier,
            inner_tokens,
            depth,
        } => Ok(vec![WherePredicate::QuantifiedSubqueryCompare {
            column,
            op: op.negated(),
            quantifier: quantifier.flipped(),
            inner_tokens,
            depth,
        }]),
        // 量化比較の否定は量化子の反転と演算子の否定（`NOT (x = ANY a)` = `x <> ALL a`）。量化子の
        // De Morgan は Kleene の三値論理でも厳密に成り立つ（空配列・NULL 要素・左辺 NULL を含む。
        // TABLE-35・Issue #1603）。
        WherePredicate::Expression(Expr::Quantified {
            op,
            quantifier,
            lhs,
            rhs,
        }) => Ok(vec![WherePredicate::Expression(Expr::Quantified {
            op: op.negated(),
            quantifier: quantifier.flipped(),
            lhs,
            rhs,
        })]),
        // `IS [NOT] DISTINCT FROM` は結果が NULL にならない（常に真偽値）ため、`negated` の反転が
        // 三値論理でも厳密な否定になる（SQL-54・Issue #1783）。
        WherePredicate::Expression(Expr::IsDistinctFrom { negated, lhs, rhs }) => {
            Ok(vec![WherePredicate::Expression(Expr::IsDistinctFrom {
                negated: !negated,
                lhs,
                rhs,
            })])
        }
        // 正規表現マッチは左辺・パターンのどちらかが NULL なら否定の有無に関わらず NULL なので、
        // `negated` の反転が三値論理でも同値（SQL-55・Issue #1787）。
        WherePredicate::Expression(Expr::RegexMatch {
            negated,
            case_insensitive,
            lhs,
            pattern,
            raw_pattern_len,
        }) => Ok(vec![WherePredicate::Expression(Expr::RegexMatch {
            negated: !negated,
            case_insensitive,
            lhs,
            pattern,
            raw_pattern_len,
        })]),
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
        WherePredicate::Expression(_)
        | WherePredicate::IdCompare { .. }
        | WherePredicate::CorrelatedExists(_) => Err(SqlSurfaceError::unsupported(
            "NOT must be followed by a comparison",
        )),
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

    fn distinct(negated: bool) -> WherePredicate {
        WherePredicate::Expression(Expr::IsDistinctFrom {
            negated,
            lhs: Box::new(Expr::Ident("a".to_string())),
            rhs: Box::new(Expr::Ident("b".to_string())),
        })
    }

    /// SQL-54: `NOT (a IS DISTINCT FROM b)` は `negated` の反転で、二重否定は元へ戻る。
    #[test]
    fn is_distinct_from_flips_negated_and_double_negation_restores() {
        let mut b = 100;
        let once = negate_conjunction(vec![distinct(false)], &mut b).unwrap();
        assert_eq!(once, vec![distinct(true)]);
        let twice = negate_conjunction(once, &mut b).unwrap();
        assert_eq!(twice, vec![distinct(false)]);
    }

    fn regex(negated: bool) -> WherePredicate {
        WherePredicate::Expression(Expr::RegexMatch {
            negated,
            case_insensitive: true,
            lhs: Box::new(Expr::Ident("a".to_string())),
            pattern: Box::new(Expr::String("x".to_string())),
            raw_pattern_len: None,
        })
    }

    /// SQL-55: `NOT (a ~* 'x')` は `negated` の反転で、二重否定・De Morgan でも保たれる。
    #[test]
    fn regex_match_flips_negated() {
        let mut b = 100;
        let once = negate_conjunction(vec![regex(false)], &mut b).unwrap();
        assert_eq!(once, vec![regex(true)]);
        assert_eq!(
            negate_conjunction(once, &mut b).unwrap(),
            vec![regex(false)]
        );
        let out = negate_conjunction(vec![regex(false), regex(true)], &mut b).unwrap();
        assert_eq!(
            out,
            vec![WherePredicate::Or(vec![
                vec![regex(true)],
                vec![regex(false)]
            ])]
        );
    }

    /// SQL-54: 連言の否定（De Morgan）でも各項の `negated` が反転される。
    #[test]
    fn is_distinct_from_under_de_morgan() {
        let mut b = 100;
        let out = negate_conjunction(vec![distinct(false), distinct(true)], &mut b).unwrap();
        assert_eq!(
            out,
            vec![WherePredicate::Or(vec![
                vec![distinct(true)],
                vec![distinct(false)]
            ])]
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

    /// TABLE-35・Issue #1603: 量化比較の否定は量化子の反転と演算子の否定（二重否定で元に戻る）。
    #[test]
    fn quantified_negation_flips_quantifier_and_operator() {
        use crate::sql::udf_call::{ArrayCmpOp, Quantifier};
        let q = |op, quantifier| {
            WherePredicate::Expression(Expr::Quantified {
                op,
                quantifier,
                lhs: Box::new(Expr::Number("1".into())),
                rhs: Box::new(Expr::Ident("a".into())),
            })
        };
        let mut b = 100;
        for (op, neg) in [
            (ArrayCmpOp::Eq, ArrayCmpOp::Ne),
            (ArrayCmpOp::Lt, ArrayCmpOp::Ge),
            (ArrayCmpOp::Le, ArrayCmpOp::Gt),
        ] {
            let once =
                negate_conjunction(vec![q(op, Quantifier::Any)], &mut b).expect("negate once");
            assert_eq!(once, vec![q(neg, Quantifier::All)]);
            let twice = negate_conjunction(once, &mut b).expect("negate twice");
            assert_eq!(twice, vec![q(op, Quantifier::Any)]);
        }
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

    /// Issue #1855・SQL-61 (3): サブクエリ形の量化比較の否定は演算子と量化子の両方を反転し、
    /// 二重否定で元に戻る。
    #[test]
    fn quantified_subquery_negation_flips_operator_and_quantifier() {
        use crate::sql::allowlist::ScalarSubqueryOp as O;
        use crate::sql::udf_call::Quantifier as Q;
        let mut b = 100;
        let p = |op, quantifier| WherePredicate::QuantifiedSubqueryCompare {
            column: "c".into(),
            op,
            quantifier,
            inner_tokens: vec![],
            depth: 1,
        };
        for (op, nop) in [
            (O::Eq, O::Ne),
            (O::Lt, O::Ge),
            (O::Le, O::Gt),
            (O::Gt, O::Le),
            (O::Ge, O::Lt),
            (O::Ne, O::Eq),
        ] {
            for (q, nq) in [(Q::Any, Q::All), (Q::All, Q::Any)] {
                assert_eq!(
                    negate_conjunction(vec![p(op, q)], &mut b).unwrap(),
                    vec![p(nop, nq)]
                );
                assert_eq!(
                    negate_conjunction(vec![p(nop, nq)], &mut b).unwrap(),
                    vec![p(op, q)]
                );
            }
        }
    }

    /// Issue #1526・SQL-41: 空の連言（恒真）の否定は恒偽、逆も成り立つ。
    #[test]
    fn empty_conjunction_negates_to_false_and_back() {
        let mut b = 100;
        let falsum = vec![WherePredicate::Or(Vec::new())];
        assert_eq!(negate_conjunction(vec![], &mut b).unwrap(), falsum);
        assert_eq!(negate_conjunction(falsum, &mut b).unwrap(), vec![]);
    }
}
