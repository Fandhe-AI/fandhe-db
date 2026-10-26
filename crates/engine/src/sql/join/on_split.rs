//! `JOIN ... ON` の論理木を結合キーと残余述語へ分解する分類器（Issue #1829・#1839・
//! SQL-60 ポインタ）。
//!
//! 責務境界: 構文段（`sql::allowlist`）が保持した ON の論理木
//! （[`JoinWhereExpr`]。最上位 `AND` の conjunct 単位）を、束縛段 `plan::build_plan` の結合段ごとに分類する。
//! 型には依存しない構造分類のみを担い、キーの型クラス一致検査（`42804`）は呼び出し元
//! （`build_plan`）が `StepKey` 化の際に行う。
//!
//! 分類（`acc` = これまでの結合結果、`new` = 新しく加わる relation）:
//! - [`OnClass::EquiKey`]: 一方が `acc` 側 relation、他方が `new` の列である `=` 比較
//! - [`OnClass::AccOnly`]: 参照が `acc` 側 relation だけ（`new` を含まない）
//! - [`OnClass::NewOnly`]: 参照が `new` だけ
//! - [`OnClass::Mixed`]: 両側を跨ぐ非キー（`a.x > b.y` 等）
//!
//! `OR` を含む conjunct は結合キーにならず、参照 relation 集合から残余（左のみ・右のみ・
//! 混在）にだけ分類する（Issue #1839）。
//!
//! #1830 との契約: [`OnSplit`] の各 `Vec` は宣言順を保つ。残余（左のみ・右のみ・
//! 混在）は `build_plan` が `StepPlan` の `on_*` へ束縛し、`exec::join_step` が一致条件の
//! 一部として評価する（述語を黙殺する fail-open を作らない）。外部結合の ON 残余は WHERE の
//! strict マスク簡約へ流さない（ON 述語は NULL 補完の有無を決めるのみ）。

use crate::sql::allowlist::{JoinWhereExpr, JoinWherePredicate, SqlSurfaceError};
use crate::sql::relation::ResolvedColumn;
use crate::sql::udf_call::BinOp;

use super::plan::{collect_leaf_colrefs, flatten_conjuncts, relation_bit};
use super::using::JoinScope;

/// ON の葉 1 個の分類結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OnClass {
    /// 等価結合キー。`lhs_is_new` は葉の左辺が `new` 側の列であること。
    EquiKey {
        lhs_is_new: bool,
    },
    AccOnly,
    NewOnly,
    Mixed,
}

/// 分類済みの ON。`keys` は `(acc 側, new 側)` に正規化した解決列。
pub(super) struct OnSplit<'p> {
    pub(super) keys: Vec<(ResolvedColumn, ResolvedColumn)>,
    pub(super) acc_only: Vec<&'p JoinWhereExpr>,
    pub(super) new_only: Vec<&'p JoinWhereExpr>,
    pub(super) mixed: Vec<&'p JoinWhereExpr>,
}

#[cfg(test)]
impl OnSplit<'_> {
    /// 等価キー以外の残余述語が 1 つでもあるか（分類の単体テスト用）。
    pub(super) fn has_residual(&self) -> bool {
        !(self.acc_only.is_empty() && self.new_only.is_empty() && self.mixed.is_empty())
    }
}

/// 葉が参照する relation 添字列（`rels`、葉内の列参照順）だけで葉を分類する純関数。
pub(super) fn classify_leaf(
    pred: &JoinWherePredicate,
    rels: &[usize],
    new_rel: usize,
) -> Result<OnClass, SqlSurfaceError> {
    if let (JoinWherePredicate::ColumnCompare { op: BinOp::Eq, .. }, [l, r]) = (pred, rels) {
        match (*l == new_rel, *r == new_rel) {
            (false, true) => return Ok(OnClass::EquiKey { lhs_is_new: false }),
            (true, false) => return Ok(OnClass::EquiKey { lhs_is_new: true }),
            _ => {}
        }
    }
    classify_refs(rels, new_rel)
}

/// 参照 relation 添字の集合だけで残余の側（左のみ・右のみ・混在）を決める。
fn classify_refs(rels: &[usize], new_rel: usize) -> Result<OnClass, SqlSurfaceError> {
    let new_bit = relation_bit(new_rel)?;
    let mut mask = 0u32;
    for r in rels {
        mask |= relation_bit(*r)?;
    }
    Ok(if mask & new_bit == 0 {
        OnClass::AccOnly
    } else if mask == new_bit {
        OnClass::NewOnly
    } else {
        OnClass::Mixed
    })
}

/// ON の論理木を分類する。最上位の `AND` を conjunct に平坦化し、全 conjunct の全列参照を
/// 分類より前に解決する（未知列 `42703`・未知修飾子 `42P01`・曖昧列等を WHERE 経路と
/// 同じ順序で確定する）。葉の conjunct は [`classify_leaf`]、`OR` を含む conjunct は
/// 結合キーにせず、参照 relation 集合から残余（左のみ・右のみ・混在）へ分類する
/// （Issue #1839。`OR` は残余評価器 `build_residual` が束縛する）。
pub(super) fn classify_on<'p>(
    on: &'p JoinWhereExpr,
    scope: &JoinScope<'_>,
    new_rel: usize,
) -> Result<OnSplit<'p>, SqlSurfaceError> {
    let mut conjuncts: Vec<&'p JoinWhereExpr> = Vec::new();
    flatten_conjuncts(on, &mut conjuncts);
    let mut resolved: Vec<Vec<ResolvedColumn>> = Vec::with_capacity(conjuncts.len());
    for c in &conjuncts {
        let mut refs = Vec::new();
        collect_leaf_colrefs(c, &mut refs);
        let mut cols = Vec::with_capacity(refs.len());
        for r in refs {
            cols.push(scope.resolve(r)?);
        }
        resolved.push(cols);
    }
    let mut out = OnSplit {
        keys: Vec::new(),
        acc_only: Vec::new(),
        new_only: Vec::new(),
        mixed: Vec::new(),
    };
    for (conj, cols) in conjuncts.into_iter().zip(resolved) {
        let rels: Vec<usize> = cols.iter().map(ResolvedColumn::relation).collect();
        let class = match conj {
            JoinWhereExpr::Leaf(leaf) => classify_leaf(leaf, &rels, new_rel)?,
            // 非葉（OR の部分木）は等価キーにしない。参照 relation 集合だけで分類する。
            _ => classify_refs(&rels, new_rel)?,
        };
        match class {
            OnClass::EquiKey { lhs_is_new } => {
                let mut it = cols.into_iter();
                match (it.next(), it.next()) {
                    (Some(l), Some(r)) => out.keys.push(if lhs_is_new { (r, l) } else { (l, r) }),
                    _ => {
                        return Err(SqlSurfaceError::Internal {
                            detail: "JOIN ON key leaf lost a column".to_string(),
                        })
                    }
                }
            }
            OnClass::AccOnly => out.acc_only.push(conj),
            OnClass::NewOnly => out.new_only.push(conj),
            OnClass::Mixed => out.mixed.push(conj),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    //! 分類器の単体テスト（Issue #1829・SQL-60 ポインタ）。DB なしで構築する。
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::sql::allowlist::CompareOp;
    use crate::sql::relation::{BindingScope, ColumnRef, TableRef};

    fn schema(name: &str) -> TableSchema {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("x", ColumnType::Text, false),
                ColumnDef::new("y", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, false),
                ColumnDef::new("active", ColumnType::Boolean, false),
            ],
        )
    }

    fn cc(l: (&str, &str), op: BinOp, r: (&str, &str)) -> JoinWherePredicate {
        JoinWherePredicate::ColumnCompare {
            lhs: ColumnRef::qualified(l.0, l.1),
            op,
            rhs: ColumnRef::qualified(r.0, r.1),
        }
    }

    fn and_of(conds: &[JoinWherePredicate]) -> JoinWhereExpr {
        JoinWhereExpr::And(conds.iter().cloned().map(JoinWhereExpr::Leaf).collect())
    }

    fn leaf(p: JoinWherePredicate) -> JoinWhereExpr {
        JoinWhereExpr::Leaf(p)
    }

    /// `names` の relation を並べたスコープで木を分類し、`(keys, acc, new, mixed)` 件数を返す。
    fn run_tree(
        names: &[&str],
        on: &JoinWhereExpr,
    ) -> Result<(usize, usize, usize, usize), SqlSurfaceError> {
        let schemas: Vec<TableSchema> = names.iter().map(|n| schema(n)).collect();
        let scope = JoinScope::plain(BindingScope::new(
            names
                .iter()
                .map(|n| TableRef::new(*n))
                .zip(schemas.iter())
                .collect(),
        )?);
        let s = classify_on(on, &scope, names.len() - 1)?;
        Ok((
            s.keys.len(),
            s.acc_only.len(),
            s.new_only.len(),
            s.mixed.len(),
        ))
    }

    #[test]
    fn or_conjunct_is_residual_only_never_a_key() {
        // Issue #1839: OR の部分木は等価葉を含んでも結合キーにならず、混在残余になる。
        let on = JoinWhereExpr::Or(vec![
            leaf(cc(("a", "x"), BinOp::Eq, ("b", "y"))),
            leaf(cc(("a", "x"), BinOp::Gt, ("b", "y"))),
        ]);
        assert_eq!(run_tree(&["a", "b"], &on).unwrap(), (0, 0, 0, 1));
        // 単側だけの OR は右のみ／左のみ残余。
        let on = JoinWhereExpr::Or(vec![
            leaf(eq(("b", "tag"), "x")),
            leaf(eq(("b", "tag"), "z")),
        ]);
        assert_eq!(run_tree(&["a", "b"], &on).unwrap(), (0, 0, 1, 0));
        let on = JoinWhereExpr::Or(vec![
            leaf(eq(("a", "tag"), "x")),
            leaf(eq(("a", "tag"), "z")),
        ]);
        assert_eq!(run_tree(&["a", "b"], &on).unwrap(), (0, 1, 0, 0));
    }

    #[test]
    fn nested_and_yields_key_and_or_residual() {
        // ON (a.x = b.y AND a.x < b.y) AND (a.tag = 'x' OR b.tag = 'x')
        let on = JoinWhereExpr::And(vec![
            JoinWhereExpr::And(vec![
                leaf(cc(("a", "x"), BinOp::Eq, ("b", "y"))),
                leaf(cc(("a", "x"), BinOp::Lt, ("b", "y"))),
            ]),
            JoinWhereExpr::Or(vec![
                leaf(eq(("a", "tag"), "x")),
                leaf(eq(("b", "tag"), "x")),
            ]),
        ]);
        assert_eq!(run_tree(&["a", "b"], &on).unwrap(), (1, 0, 0, 2));
    }

    #[test]
    fn unknown_column_inside_or_is_42703_before_classification() {
        let on = JoinWhereExpr::Or(vec![
            leaf(eq(("b", "tag"), "x")),
            leaf(eq(("b", "nope"), "x")),
        ]);
        let e = run_tree(&["a", "b"], &on).unwrap_err();
        assert_eq!(e.wire_code(), "42703");
    }

    fn eq(c: (&str, &str), v: &str) -> JoinWherePredicate {
        JoinWherePredicate::Equality {
            column: ColumnRef::qualified(c.0, c.1),
            value: v.to_string(),
        }
    }

    /// `(keys, acc_only, new_only, mixed 件数, keys の (acc, new) relation 添字)`。
    type Counts = (usize, usize, usize, usize, Vec<(usize, usize)>);

    /// `names` の relation を並べたスコープで `classify_on` を実行する。
    fn run(names: &[&str], conds: &[JoinWherePredicate]) -> Result<Counts, SqlSurfaceError> {
        let schemas: Vec<TableSchema> = names.iter().map(|n| schema(n)).collect();
        let scope = JoinScope::plain(BindingScope::new(
            names
                .iter()
                .map(|n| TableRef::new(*n))
                .zip(schemas.iter())
                .collect(),
        )?);
        let on = and_of(conds);
        let s = classify_on(&on, &scope, names.len() - 1)?;
        Ok((
            s.keys.len(),
            s.acc_only.len(),
            s.new_only.len(),
            s.mixed.len(),
            s.keys
                .iter()
                .map(|(a, n)| (a.relation(), n.relation()))
                .collect(),
        ))
    }

    #[test]
    fn equi_key_is_normalized_in_either_order() {
        let c = [cc(("a", "x"), BinOp::Eq, ("b", "y"))];
        assert_eq!(run(&["a", "b"], &c).unwrap(), (1, 0, 0, 0, vec![(0, 1)]));
        let c = [cc(("b", "y"), BinOp::Eq, ("a", "x"))];
        assert_eq!(run(&["a", "b"], &c).unwrap(), (1, 0, 0, 0, vec![(0, 1)]));
    }

    #[test]
    fn acc_only_leaves() {
        let c = [eq(("a", "tag"), "x"), cc(("a", "x"), BinOp::Eq, ("a", "y"))];
        assert_eq!(run(&["a", "b"], &c).unwrap(), (0, 2, 0, 0, vec![]));
        // 3 方向で acc 側 relation 同士の比較（new = c）。
        let c = [cc(("a", "x"), BinOp::Eq, ("b", "y"))];
        assert_eq!(run(&["a", "b", "c"], &c).unwrap(), (0, 1, 0, 0, vec![]));
    }

    #[test]
    fn new_only_leaves() {
        let c = [
            eq(("b", "tag"), "x"),
            JoinWherePredicate::InList {
                column: ColumnRef::qualified("b", "x"),
                values: vec!["p".into(), "q".into()],
            },
            JoinWherePredicate::BoolColumn {
                column: ColumnRef::qualified("b", "active"),
            },
            cc(("b", "x"), BinOp::Eq, ("b", "y")),
        ];
        assert_eq!(run(&["a", "b"], &c).unwrap(), (0, 0, 4, 0, vec![]));
    }

    #[test]
    fn mixed_leaves() {
        let c = [
            cc(("a", "x"), BinOp::Gt, ("b", "y")),
            cc(("a", "x"), BinOp::Le, ("b", "y")),
        ];
        assert_eq!(run(&["a", "b"], &c).unwrap(), (0, 0, 0, 2, vec![]));
        let c = [JoinWherePredicate::Compare {
            column: ColumnRef::qualified("b", "x"),
            op: CompareOp::Gt,
            value: "v".into(),
        }];
        assert_eq!(run(&["a", "b"], &c).unwrap(), (0, 0, 1, 0, vec![]));
    }

    #[test]
    fn composite_keeps_declaration_order() {
        let c = [
            cc(("a", "x"), BinOp::Eq, ("b", "y")),
            eq(("b", "tag"), "x"),
            cc(("a", "x"), BinOp::Gt, ("b", "y")),
            eq(("a", "tag"), "z"),
        ];
        let schemas = [schema("a"), schema("b")];
        let scope = JoinScope::plain(
            BindingScope::new(vec![
                (TableRef::new("a"), &schemas[0]),
                (TableRef::new("b"), &schemas[1]),
            ])
            .unwrap(),
        );
        let on = and_of(&c);
        let s = classify_on(&on, &scope, 1).unwrap();
        assert_eq!(
            (
                s.keys.len(),
                s.acc_only.len(),
                s.new_only.len(),
                s.mixed.len()
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(s.new_only[0], &JoinWhereExpr::Leaf(c[1].clone()));
        assert_eq!(s.mixed[0], &JoinWhereExpr::Leaf(c[2].clone()));
        assert_eq!(s.acc_only[0], &JoinWhereExpr::Leaf(c[3].clone()));
        assert!(s.has_residual());
    }

    #[test]
    fn no_key_yields_empty_keys() {
        let c = [cc(("a", "x"), BinOp::Gt, ("b", "y"))];
        assert_eq!(run(&["a", "b"], &c).unwrap().0, 0);
    }

    #[test]
    fn resolution_errors_come_first() {
        let c = [eq(("b", "tag"), "x"), eq(("b", "nonexistent"), "x")];
        let e = run(&["a", "b"], &c).unwrap_err();
        assert_eq!(e.wire_code(), "42703");
        // 未出現の relation への前方参照は 42P01。
        let c = [cc(("a", "x"), BinOp::Eq, ("c", "y"))];
        let e = run(&["a", "b"], &c).unwrap_err();
        assert_eq!(e.wire_code(), "42P01");
    }
}
