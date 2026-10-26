//! `GROUP BY` の `ROLLUP`・`CUBE`・`GROUPING SETS` の内部表現と展開（Issue #1821・SQL-59 の
//! ポインタ）。
//!
//! 役割: 構文段（[`crate::sql::allowlist`] の `Parser::parse_group_by_clause`）が読み取った
//! 「キー集合の指定の木」を、グルーピングセットの列（各セットは全キーの一意列への添字の
//! 昇順・重複なしの列）へ正規化する純粋なロジックを担う。集約の実行・小計行の NULL 合成は
//! 後続の実行段（Issue #1822）の責務で、本モジュールは行にも catalog にも触れない。
//!
//! 呼び出し順（確保の前に検査する fail-closed の設計）:
//! 1. 構文段が `CUBE` の要素数（[`MAX_CUBE_ELEMENTS`]）と入れ子の深さ
//!    （[`MAX_GROUPING_SETS_DEPTH`]）を読みながら検査する。
//! 2. [`expand_grouping`] が、木の形だけから展開後のセット数を飽和演算で数え
//!    （[`MAX_GROUPING_SETS`] 超過は `54001`）、次に一意なキー数を既存の
//!    [`check_group_by_column_count`] で検査し（`54000`）、最後に具体化する。
//!
//! 上限エラーは文の形だけで決まり、行・テナントに依存しない（他テナントの存在を漏らさない）。
//! 数値の出典は SQL-59（ポインタ表記のみ。spec 本文は転記しない）。

use crate::sql::allowlist::{check_group_by_column_count, SqlSurfaceError};
use crate::sql::udf_call::Expr;

/// 展開後のグルーピングセット数の上限（Issue #1821・SQL-59。PostgreSQL 17 と同値）。
pub(crate) const MAX_GROUPING_SETS: usize = 4096;

/// `CUBE` の要素数の上限（Issue #1821・SQL-59。PostgreSQL 17 と同値）。超過は `54011`。
pub(crate) const MAX_CUBE_ELEMENTS: usize = 12;

/// `GROUPING SETS` の入れ子の深さの上限（Issue #1821。再帰の暴走を防ぐ本実装の固定値）。
/// 超過は `54001`。
pub(crate) const MAX_GROUPING_SETS_DEPTH: usize = 8;

/// グルーピングの指定に現れたキー 1 つ（添字化の前）。`Column` は裸の列名（`(col)` の括弧は
/// 正規化済み）、`Expr` は式キー（Issue #1817・SQL-58）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RawGroupKey {
    Column(String),
    Expr(Expr),
}

/// キー出現のプール添字（構文段が出現順に積む。同じキーの重複出現は別添字で、一意化は
/// [`expand_grouping`] が行う）。
pub(crate) type KeyId = usize;

/// グルーピング指定の木の 1 要素（`GROUP BY` のトップレベル項目、または `GROUPING SETS` の要素）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GroupingElement {
    /// 単一キー 1 つから成るセット。
    Key(KeyId),
    /// 複合要素 `(a, b)`。1 つのセットになる。
    Composite(Vec<KeyId>),
    /// 空のセット `()`（総計）。
    Empty,
    /// `ROLLUP(e1..en)`。各要素は単一キーまたは複合要素のキー群。
    Rollup(Vec<Vec<KeyId>>),
    /// `CUBE(e1..en)`。要素数は構文段が [`MAX_CUBE_ELEMENTS`] 以下に制限済み。
    Cube(Vec<Vec<KeyId>>),
    /// `GROUPING SETS (...)`。入れ子の `GROUPING SETS`・`ROLLUP`・`CUBE` は要素として現れ、
    /// 展開時に平坦化される。
    Sets(Vec<GroupingElement>),
}

/// [`expand_grouping`] の結果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExpandedGrouping {
    /// 全セットに現れるキーの和集合（初出順に一意化）。`sets` の添字はこの配列を指す。
    pub(crate) keys: Vec<RawGroupKey>,
    /// 展開後のグルーピングセット。各セットは `keys` への添字の昇順・重複なし。空のセットは総計。
    /// 同一のセットが複数できた場合は保持する（PostgreSQL と同じ。行の合成は #1822）。
    pub(crate) sets: Vec<Vec<usize>>,
}

fn too_many_sets() -> SqlSurfaceError {
    SqlSurfaceError::StatementTooComplex {
        detail: "too many grouping sets present (maximum 4096)",
    }
}

/// 木 1 要素の展開後セット数を、`MAX_GROUPING_SETS + 1` で頭打ちにして数える（飽和演算。
/// オーバーフローも超過として扱う）。
fn count_element(el: &GroupingElement) -> usize {
    let cap = MAX_GROUPING_SETS.saturating_add(1);
    match el {
        GroupingElement::Key(_) | GroupingElement::Composite(_) | GroupingElement::Empty => 1,
        GroupingElement::Rollup(v) => v.len().saturating_add(1).min(cap),
        GroupingElement::Cube(v) => {
            // 構文段が 12 要素以下を保証するが、多層防御として桁あふれ時は頭打ちの値にする。
            u32::try_from(v.len())
                .ok()
                .and_then(|n| 1usize.checked_shl(n))
                .map_or(cap, |c| c.min(cap))
        }
        GroupingElement::Sets(children) => children
            .iter()
            .fold(0usize, |acc, c| acc.saturating_add(count_element(c)))
            .min(cap),
    }
}

/// トップレベル項目列の展開後セット数（各項目のセット数の積）を数える。確保は一切しない。
/// 上限超過は `54001`。
pub(crate) fn count_sets(items: &[GroupingElement]) -> Result<usize, SqlSurfaceError> {
    let cap = MAX_GROUPING_SETS.saturating_add(1);
    let mut total = 1usize;
    for item in items {
        total = total.saturating_mul(count_element(item)).min(cap);
        if total > MAX_GROUPING_SETS {
            return Err(too_many_sets());
        }
    }
    Ok(total)
}

/// プールのキーを初出順に一意化し、`pool` の添字から一意キー添字への写像を返す。一意キー数は
/// [`check_group_by_column_count`]（`54000`）で、新しいキーを `push` する前に検査する。
fn intern_keys(pool: Vec<RawGroupKey>) -> Result<(Vec<RawGroupKey>, Vec<usize>), SqlSurfaceError> {
    let mut unique: Vec<RawGroupKey> = Vec::new();
    let mut map: Vec<usize> = Vec::with_capacity(pool.len());
    for key in pool {
        match unique.iter().position(|k| *k == key) {
            Some(i) => map.push(i),
            None => {
                check_group_by_column_count(unique.len().saturating_add(1))?;
                map.push(unique.len());
                unique.push(key);
            }
        }
    }
    Ok((unique, map))
}

/// 一意キー添字の昇順・重複なしの列へ正規化して併合する。
fn union_into(
    target: &mut Vec<usize>,
    ids: &[KeyId],
    map: &[usize],
) -> Result<(), SqlSurfaceError> {
    for id in ids {
        // 構文段が積んだ添字は必ずプール内。範囲外は内部不整合として fail-closed にする。
        let Some(&u) = map.get(*id) else {
            return Err(SqlSurfaceError::unsupported(
                "invalid grouping key reference",
            ));
        };
        if let Err(pos) = target.binary_search(&u) {
            target.insert(pos, u);
        }
    }
    Ok(())
}

/// 1 要素を、展開後のセット列（宣言順）へ具体化する。呼び出し前に [`count_sets`] で総数が
/// 上限内であることを検証済みであること。
fn sets_of(el: &GroupingElement, map: &[usize]) -> Result<Vec<Vec<usize>>, SqlSurfaceError> {
    match el {
        GroupingElement::Key(id) => {
            let mut s = Vec::new();
            union_into(&mut s, std::slice::from_ref(id), map)?;
            Ok(vec![s])
        }
        GroupingElement::Composite(ids) => {
            let mut s = Vec::new();
            union_into(&mut s, ids, map)?;
            Ok(vec![s])
        }
        GroupingElement::Empty => Ok(vec![Vec::new()]),
        GroupingElement::Rollup(elems) => {
            // 長い接頭辞から順に `(e1..en), ..., (e1), ()`。
            let mut out = Vec::with_capacity(elems.len().saturating_add(1));
            for k in (0..=elems.len()).rev() {
                let mut s = Vec::new();
                for e in elems.iter().take(k) {
                    union_into(&mut s, e, map)?;
                }
                out.push(s);
            }
            Ok(out)
        }
        GroupingElement::Cube(elems) => {
            // 第 1 要素を最上位ビットとするマスクを全 1 から 0 へ降順にたどる
            // （`CUBE(a,b,c)` は `(a,b,c),(a,b),(a,c),(a),(b,c),(b),(c),()`）。
            let n = elems.len();
            let Some(total) = u32::try_from(n).ok().and_then(|n| 1usize.checked_shl(n)) else {
                return Err(too_many_sets());
            };
            let mut out = Vec::with_capacity(total.min(MAX_GROUPING_SETS));
            for mask in (0..total).rev() {
                let mut s = Vec::new();
                for (i, e) in elems.iter().enumerate() {
                    let bit = n - 1 - i;
                    if (mask >> bit) & 1 == 1 {
                        union_into(&mut s, e, map)?;
                    }
                }
                out.push(s);
            }
            Ok(out)
        }
        GroupingElement::Sets(children) => {
            let mut out = Vec::new();
            for c in children {
                out.extend(sets_of(c, map)?);
            }
            Ok(out)
        }
    }
}

/// 構文段が読み取った項目列を、一意キー列とグルーピングセットの列へ展開する。判定順は
/// セット数（`54001`）→ 一意キー数（`54000`）→ 具体化。トップレベルの項目は直積（左の項目を
/// 外側のループ）の和集合で 1 セットになる。
pub(crate) fn expand_grouping(
    pool: Vec<RawGroupKey>,
    items: &[GroupingElement],
) -> Result<ExpandedGrouping, SqlSurfaceError> {
    let total = count_sets(items)?;
    let (keys, map) = intern_keys(pool)?;
    let mut acc: Vec<Vec<usize>> = Vec::with_capacity(total);
    acc.push(Vec::new());
    for item in items {
        let item_sets = sets_of(item, &map)?;
        let mut next: Vec<Vec<usize>> =
            Vec::with_capacity(acc.len().saturating_mul(item_sets.len()).min(total));
        for a in &acc {
            for b in &item_sets {
                let mut merged = a.clone();
                for u in b {
                    if let Err(pos) = merged.binary_search(u) {
                        merged.insert(pos, *u);
                    }
                }
                next.push(merged);
            }
        }
        acc = next;
    }
    Ok(ExpandedGrouping { keys, sets: acc })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_format::ClassifiedError;

    /// 列名 `names` を 1 つずつプールに積み、`Key` 要素の添字をそのまま返す補助。
    fn pool_of(names: &[&str]) -> Vec<RawGroupKey> {
        names
            .iter()
            .map(|n| RawGroupKey::Column((*n).to_owned()))
            .collect()
    }

    fn ids(v: &[usize]) -> Vec<KeyId> {
        v.to_vec()
    }

    fn singles(v: &[usize]) -> Vec<Vec<KeyId>> {
        v.iter().map(|i| vec![*i]).collect()
    }

    fn expand(names: &[&str], items: &[GroupingElement]) -> ExpandedGrouping {
        expand_grouping(pool_of(names), items).expect("expand")
    }

    fn wire_code(e: &SqlSurfaceError) -> &'static str {
        e.error_class().wire_code()
    }

    #[test]
    fn rollup_expands_to_prefixes() {
        let r = expand(
            &["a", "b", "c"],
            &[GroupingElement::Rollup(singles(&[0, 1, 2]))],
        );
        assert_eq!(r.sets, vec![vec![0, 1, 2], vec![0, 1], vec![0], vec![]]);
    }

    #[test]
    fn cube_expands_to_power_set_in_documented_order() {
        let r = expand(&["a", "b"], &[GroupingElement::Cube(singles(&[0, 1]))]);
        assert_eq!(r.sets, vec![vec![0, 1], vec![0], vec![1], vec![]]);
        let r = expand(
            &["a", "b", "c"],
            &[GroupingElement::Cube(singles(&[0, 1, 2]))],
        );
        assert_eq!(
            r.sets,
            vec![
                vec![0, 1, 2],
                vec![0, 1],
                vec![0, 2],
                vec![0],
                vec![1, 2],
                vec![1],
                vec![2],
                vec![]
            ]
        );
    }

    #[test]
    fn grouping_sets_concatenate_and_flatten_nested() {
        let r = expand(
            &["a", "b"],
            &[GroupingElement::Sets(vec![
                GroupingElement::Composite(ids(&[0, 1])),
                GroupingElement::Key(0),
                GroupingElement::Empty,
            ])],
        );
        assert_eq!(r.sets, vec![vec![0, 1], vec![0], vec![]]);
        // 入れ子の GROUPING SETS・ROLLUP・CUBE は平坦化される。
        let r = expand(
            &["a", "b"],
            &[GroupingElement::Sets(vec![
                GroupingElement::Key(0),
                GroupingElement::Sets(vec![GroupingElement::Key(1), GroupingElement::Empty]),
                GroupingElement::Rollup(singles(&[0, 1])),
                GroupingElement::Cube(singles(&[0])),
            ])],
        );
        assert_eq!(
            r.sets,
            vec![
                vec![0],
                vec![1],
                vec![],
                vec![0, 1],
                vec![0],
                vec![],
                vec![0],
                vec![]
            ]
        );
    }

    #[test]
    fn composite_elements_in_rollup() {
        let r = expand(
            &["a", "b", "c"],
            &[GroupingElement::Rollup(vec![vec![0, 1], vec![2]])],
        );
        assert_eq!(r.sets, vec![vec![0, 1, 2], vec![0, 1], vec![]]);
    }

    #[test]
    fn top_level_items_form_cartesian_product() {
        let r = expand(
            &["a", "b", "c"],
            &[
                GroupingElement::Key(0),
                GroupingElement::Rollup(singles(&[1, 2])),
            ],
        );
        assert_eq!(r.sets, vec![vec![0, 1, 2], vec![0, 1], vec![0]]);
        let r = expand(
            &["a", "b"],
            &[
                GroupingElement::Rollup(singles(&[0])),
                GroupingElement::Cube(singles(&[1])),
            ],
        );
        assert_eq!(r.sets, vec![vec![0, 1], vec![0], vec![1], vec![]]);
    }

    #[test]
    fn duplicate_keys_are_interned_and_sets_deduplicated_inside() {
        // ROLLUP(a, a): プールは 2 つだが一意キーは 1 つ。
        let r = expand(&["a", "a"], &[GroupingElement::Rollup(singles(&[0, 1]))]);
        assert_eq!(r.keys.len(), 1);
        assert_eq!(r.sets, vec![vec![0], vec![0], vec![]]);
        // a, ROLLUP(a, b)
        let r = expand(
            &["a", "a", "b"],
            &[
                GroupingElement::Key(0),
                GroupingElement::Rollup(singles(&[1, 2])),
            ],
        );
        assert_eq!(r.keys.len(), 2);
        assert_eq!(r.sets, vec![vec![0, 1], vec![0], vec![0]]);
    }

    #[test]
    fn identical_sets_are_kept() {
        let r = expand(
            &["a", "a"],
            &[GroupingElement::Sets(vec![
                GroupingElement::Key(0),
                GroupingElement::Key(1),
            ])],
        );
        assert_eq!(r.sets, vec![vec![0], vec![0]]);
    }

    #[test]
    fn empty_grouping_is_one_total_set_without_keys() {
        let r = expand(&[], &[GroupingElement::Empty]);
        assert!(r.keys.is_empty());
        assert_eq!(r.sets, vec![Vec::<usize>::new()]);
    }

    #[test]
    fn cube_of_twelve_repeated_keys_is_exactly_the_limit() {
        let pool = vec!["a"; 12];
        let all: Vec<usize> = (0..12).collect();
        let r = expand(&pool, &[GroupingElement::Cube(singles(&all))]);
        assert_eq!(r.sets.len(), MAX_GROUPING_SETS);
    }

    #[test]
    fn exceeding_set_limit_is_54001() {
        let all: Vec<usize> = (0..12).collect();
        let mut pool = vec!["a"; 12];
        pool.push("a");
        let e = expand_grouping(
            pool_of(&pool),
            &[
                GroupingElement::Cube(singles(&all)),
                GroupingElement::Rollup(singles(&[12])),
            ],
        )
        .expect_err("8192 sets");
        assert_eq!(wire_code(&e), "54001");
        // `()` を 4,097 個並べる。
        let items = vec![GroupingElement::Sets(vec![GroupingElement::Empty; 4097])];
        let e = expand_grouping(Vec::new(), &items).expect_err("4097 sets");
        assert_eq!(wire_code(&e), "54001");
        // 4,096 個ちょうどは受理される。
        let items = vec![GroupingElement::Sets(vec![GroupingElement::Empty; 4096])];
        assert_eq!(
            expand_grouping(Vec::new(), &items)
                .expect("4096")
                .sets
                .len(),
            4096
        );
    }

    #[test]
    fn huge_products_do_not_overflow() {
        let all: Vec<usize> = (0..12).collect();
        let item = GroupingElement::Cube(singles(&all));
        let items = vec![item; 64];
        let e = expand_grouping(pool_of(&["a"; 12]), &items).expect_err("overflow");
        assert_eq!(wire_code(&e), "54001");
    }

    #[test]
    fn set_limit_is_checked_before_key_limit() {
        // 9 個の別々のキー × 4,096 超のセット数は 54001（54000 ではない）。
        let names: Vec<String> = (0..9).map(|i| format!("c{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut items: Vec<GroupingElement> = (0..9).map(GroupingElement::Key).collect();
        items.push(GroupingElement::Sets(vec![GroupingElement::Empty; 4097]));
        let e = expand_grouping(pool_of(&refs), &items).expect_err("too many sets");
        assert_eq!(wire_code(&e), "54001");
    }

    #[test]
    fn nine_distinct_keys_within_set_limit_is_54000() {
        let names: Vec<String> = (0..9).map(|i| format!("c{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let items = vec![GroupingElement::Rollup(singles(
            &(0..9).collect::<Vec<_>>(),
        ))];
        let e = expand_grouping(pool_of(&refs), &items).expect_err("9 keys");
        assert_eq!(wire_code(&e), "54000");
    }
}
