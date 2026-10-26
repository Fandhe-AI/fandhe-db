//! `JOIN ... ON` の AND 連鎖を結合キーと残余述語へ分解する分類器（Issue #1829・
//! SQL-60 ポインタ）。
//!
//! 責務境界: 構文段（`sql::allowlist`）が保持した ON の葉列
//! （[`JoinWherePredicate`]）を、束縛段 `plan::build_plan` の結合段ごとに分類する。
//! 型には依存しない構造分類のみを担い、キーの型クラス一致検査（`42804`）は呼び出し元
//! （`build_plan`）が `StepKey` 化の際に行う。
//!
//! 分類（`acc` = これまでの結合結果、`new` = 新しく加わる relation）:
//! - [`OnClass::EquiKey`]: 一方が `acc` 側 relation、他方が `new` の列である `=` 比較
//! - [`OnClass::AccOnly`]: 参照が `acc` 側 relation だけ（`new` を含まない）
//! - [`OnClass::NewOnly`]: 参照が `new` だけ
//! - [`OnClass::Mixed`]: 両側を跨ぐ非キー（`a.x > b.y` 等）
//!
//! #1830 への引き継ぎ契約: [`OnSplit`] の各 `Vec` は宣言順を保つ。残余（左のみ・右のみ・
//! 混在）の評価は #1830 の責務で、現状 `build_plan` は残余を持つ ON を `0A000` で
//! 拒否する（述語を黙殺する fail-open を作らない）。外部結合の ON 残余は WHERE の
//! strict マスク簡約へ流してはならない（ON 述語は NULL 補完の有無を決めるのみ）。

use crate::sql::allowlist::{JoinWherePredicate, SqlSurfaceError};
use crate::sql::relation::ResolvedColumn;
use crate::sql::udf_call::BinOp;

use super::plan::{leaf_colrefs, relation_bit};
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
    pub(super) acc_only: Vec<&'p JoinWherePredicate>,
    pub(super) new_only: Vec<&'p JoinWherePredicate>,
    pub(super) mixed: Vec<&'p JoinWherePredicate>,
}

impl OnSplit<'_> {
    /// 等価キー以外の残余述語が 1 つでもあるか。
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

/// ON の葉列を分類する。全葉の列参照を分類より前に解決する（未知列 `42703`・未知修飾子
/// `42P01`・曖昧列等を WHERE 経路と同じ順序で確定する）。
pub(super) fn classify_on<'p>(
    conds: &'p [JoinWherePredicate],
    scope: &JoinScope<'_>,
    new_rel: usize,
) -> Result<OnSplit<'p>, SqlSurfaceError> {
    let mut resolved: Vec<Vec<ResolvedColumn>> = Vec::with_capacity(conds.len());
    for leaf in conds {
        let mut cols = Vec::new();
        for c in leaf_colrefs(leaf) {
            cols.push(scope.resolve(c)?);
        }
        resolved.push(cols);
    }
    let mut out = OnSplit {
        keys: Vec::new(),
        acc_only: Vec::new(),
        new_only: Vec::new(),
        mixed: Vec::new(),
    };
    for (leaf, cols) in conds.iter().zip(resolved) {
        let rels: Vec<usize> = cols.iter().map(ResolvedColumn::relation).collect();
        match classify_leaf(leaf, &rels, new_rel)? {
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
            OnClass::AccOnly => out.acc_only.push(leaf),
            OnClass::NewOnly => out.new_only.push(leaf),
            OnClass::Mixed => out.mixed.push(leaf),
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
        let s = classify_on(conds, &scope, names.len() - 1)?;
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
        let s = classify_on(&c, &scope, 1).unwrap();
        assert_eq!(
            (
                s.keys.len(),
                s.acc_only.len(),
                s.new_only.len(),
                s.mixed.len()
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(s.new_only[0], &c[1]);
        assert_eq!(s.mixed[0], &c[2]);
        assert_eq!(s.acc_only[0], &c[3]);
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
