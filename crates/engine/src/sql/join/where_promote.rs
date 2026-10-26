//! WHERE の等価条件を結合キーへ昇格する判定（Issue #1834・SQL-60 ポインタ）。
//!
//! 責務境界: 束縛段 `plan::build_plan` の WHERE ループから、conjunct 1 個ごとに呼ばれる
//! 純粋な構造・型判定だけを担う。行は読まない。昇格の可否のうち「conjunct の形」と
//! 「両列の型クラス」を判定し、結合段の保存側フラグ（INNER であること）の検査と
//! `StepKey` の確定は呼び出し元（`build_plan`）が行う。
//!
//! 昇格対象は、最上位 AND の直下にある `列 = 列` で、2 列が異なる relation `r1 < r2` へ
//! 解決されるもの。left-deep 連鎖では `r2` を結合する段 `r2 - 1` で両列が初めて揃うため、
//! その段の結合キー（`(acc 側 = r1 の列, new 側 = r2 の列)`）へ移す。
//!
//! 不変条件（昇格は純加算的で、新たなエラーを作らない）:
//! - 型クラス不一致・対象外型（REAL/DOUBLE/NUMERIC 等）は `None` を返し、従来の残余評価
//!   （`build_residual`）が同じ `42804`／結果を返す
//! - 等価の意味論はハッシュ照合と一致する（整数は i128 正規化、TEXT/BYTEA はバイト完全一致、
//!   ENUM は同一ラベル、NULL は不一致）。非バイト列順の照合列は `col_from` が `0A000` にする

use crate::sql::allowlist::{JoinWhereExpr, JoinWherePredicate, SqlSurfaceError};
use crate::sql::relation::ResolvedColumn;
use crate::sql::udf_call::BinOp;

use super::using::JoinScope;
use super::values::{cmp_class, key_class, JoinKeyClass};

/// 昇格できる等価条件。`acc`／`new` は結合段 `step` から見た側へ正規化済み。
pub(super) struct Promotion {
    /// 昇格先の結合段（`new` の relation 添字 - 1）。
    pub(super) step: usize,
    pub(super) acc: ResolvedColumn,
    pub(super) new: ResolvedColumn,
    pub(super) class: JoinKeyClass,
}

/// conjunct が結合キーへ昇格できる形・型なら [`Promotion`] を返す。満たさなければ `None`
/// （呼び出し元は従来のプッシュダウン／残余経路へ進む）。列解決エラーは伝播する。
pub(super) fn promotable_where_key(
    expr: &JoinWhereExpr,
    scope: &JoinScope<'_>,
) -> Result<Option<Promotion>, SqlSurfaceError> {
    let JoinWhereExpr::Leaf(JoinWherePredicate::ColumnCompare {
        lhs,
        op: BinOp::Eq,
        rhs,
    }) = expr
    else {
        return Ok(None);
    };
    let l = scope.resolve(lhs)?;
    let r = scope.resolve(rhs)?;
    let (acc, new) = match l.relation().cmp(&r.relation()) {
        std::cmp::Ordering::Less => (l, r),
        std::cmp::Ordering::Greater => (r, l),
        std::cmp::Ordering::Equal => return Ok(None),
    };
    // 比較クラスが両側一致しなければ昇格せず、残余経路が従来の誤り（42804/42601）を返す。
    match (cmp_class(acc.column_type()), cmp_class(new.column_type())) {
        (Some(a), Some(b)) if a == b => {}
        _ => return Ok(None),
    }
    // キー化できない型（REAL/DOUBLE/NUMERIC 等）は残余評価のまま。
    let (Ok(acc_class), Ok(new_class)) =
        (key_class(acc.column_type()), key_class(new.column_type()))
    else {
        return Ok(None);
    };
    if acc_class != new_class {
        return Ok(None);
    }
    let Some(step) = new.relation().checked_sub(1) else {
        return Ok(None);
    };
    Ok(Some(Promotion {
        step,
        acc,
        new,
        class: acc_class,
    }))
}

#[cfg(test)]
mod tests {
    //! 昇格判定の単体テスト（Issue #1834・SQL-60 ポインタ）。DB なしで構築する。
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::sql::relation::{BindingScope, ColumnRef, TableRef};

    fn schema(name: &str) -> TableSchema {
        TableSchema::new(
            name,
            vec![
                ColumnDef::new("x", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::BigInt, false),
                ColumnDef::new("f", ColumnType::Double, false),
            ],
        )
    }

    fn cc(l: (&str, &str), op: BinOp, r: (&str, &str)) -> JoinWhereExpr {
        JoinWhereExpr::Leaf(JoinWherePredicate::ColumnCompare {
            lhs: ColumnRef::qualified(l.0, l.1),
            op,
            rhs: ColumnRef::qualified(r.0, r.1),
        })
    }

    fn run(expr: &JoinWhereExpr) -> Result<Option<Promotion>, SqlSurfaceError> {
        let names = ["a", "b", "c"];
        let schemas: Vec<TableSchema> = names.iter().map(|n| schema(n)).collect();
        let scope = JoinScope::plain(BindingScope::new(
            names
                .iter()
                .map(|n| TableRef::new(*n))
                .zip(schemas.iter())
                .collect(),
        )?);
        promotable_where_key(expr, &scope)
    }

    #[test]
    fn eq_across_relations_is_normalized_to_acc_and_new() {
        let p = run(&cc(("b", "x"), BinOp::Eq, ("a", "x")))
            .unwrap()
            .unwrap();
        assert_eq!((p.step, p.acc.relation(), p.new.relation()), (0, 0, 1));
        let p = run(&cc(("a", "n"), BinOp::Eq, ("c", "n")))
            .unwrap()
            .unwrap();
        assert_eq!((p.step, p.acc.relation(), p.new.relation()), (1, 0, 2));
        assert_eq!(p.class, JoinKeyClass::Integer);
    }

    #[test]
    fn non_promotable_shapes_return_none() {
        // 非等価・同一 relation・OR の内側。
        assert!(run(&cc(("a", "x"), BinOp::Lt, ("b", "x")))
            .unwrap()
            .is_none());
        assert!(run(&cc(("a", "x"), BinOp::Eq, ("a", "x")))
            .unwrap()
            .is_none());
        let or = JoinWhereExpr::Or(vec![
            cc(("a", "x"), BinOp::Eq, ("b", "x")),
            cc(("a", "x"), BinOp::Eq, ("b", "x")),
        ]);
        assert!(run(&or).unwrap().is_none());
    }

    #[test]
    fn non_key_or_mismatched_types_return_none() {
        // DOUBLE 同士はキー化不可、TEXT = BIGINT は型クラス不一致（残余が 42804 を返す）。
        assert!(run(&cc(("a", "f"), BinOp::Eq, ("b", "f")))
            .unwrap()
            .is_none());
        assert!(run(&cc(("a", "x"), BinOp::Eq, ("b", "n")))
            .unwrap()
            .is_none());
    }

    #[test]
    fn unknown_column_error_propagates() {
        let e = run(&cc(("a", "nope"), BinOp::Eq, ("b", "x")))
            .err()
            .unwrap();
        assert_eq!(e.wire_code(), "42703");
    }
}
