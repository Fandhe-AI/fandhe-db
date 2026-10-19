//! `sql::index_implication` の単体テスト（INDEX-11、Issue #1655）。
//!
//! 主テストは「含意と判定した組は、実際の評価器（`BoundOrGroup::matches`。部分索引の
//! `BoundIndexPredicate::eval` と同じ経路）で Q が TRUE の行すべてで P も TRUE」を値グリッド上で
//! 確かめるオラクル（偽陽性なしの証拠）。明示の正例・負例は保守的な判定を固定する。

use super::*;
use crate::catalog::ColumnDef;
use crate::row_codec::Value;
use crate::sql::where_tree::BoundOrGroup;

fn schema() -> TableSchema {
    TableSchema::new(
        "t",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), true),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("code", ColumnType::Text, true),
            ColumnDef::new("flag", ColumnType::Boolean, true),
            ColumnDef::new("d", ColumnType::Date, true),
            ColumnDef::new(
                "amt",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            ColumnDef::new("r", ColumnType::Double, true),
            ColumnDef::new("c", ColumnType::Bpchar(4), true),
        ],
    )
}

/// 述語テキストを `WHERE` と同じ経路で束縛し、AND 連言として返す。
fn bind(text: &str) -> BoundConjunction {
    let preds = crate::sql::allowlist::parse_index_predicate_text(text)
        .unwrap_or_else(|e| panic!("{text}: {e:?}"));
    let udfs = crate::sql::udf_call::UdfRegistry::default();
    let mut budget = crate::sql::udf_call::MAX_EXPR_NODES;
    let (meta, exprs, _rls, or) =
        crate::sql::parser::bind_where_predicates(&preds, &schema(), &udfs, &mut budget, &[])
            .unwrap_or_else(|e| panic!("{text}: {e:?}"));
    BoundConjunction::new(meta, exprs, or)
}

fn implies_with(q: &str, p: &str, steps: usize) -> bool {
    let (qc, pc) = (bind(q), bind(p));
    implies_with_budget(
        &qc.metadata_filters,
        &qc.expr_filters,
        &qc.or_groups,
        &pc,
        &schema(),
        steps,
    )
}

/// INDEX-11: クエリ述語 `q` が索引述語 `p` を含意すると判定されるか。
fn implies(q: &str, p: &str) -> bool {
    implies_with(q, p, MAX_IMPLICATION_STEPS)
}

fn day(s: &str) -> i32 {
    crate::datetime::parse_date(s).expect("date")
}

fn dec(s: &str) -> crate::numeric::Decimal {
    crate::numeric::parse_for_column(s, 10, 2).expect("decimal")
}

/// グリッドの 1 行（エンコード済み）と行 `id`。
fn grid() -> Vec<(u64, Vec<u8>)> {
    let ns = [None, Some(-1), Some(0), Some(4), Some(5), Some(6), Some(11)];
    let codes = [None, Some("a"), Some("b"), Some("c")];
    let flags = [None, Some(true), Some(false)];
    let dates = [None, Some("2023-06-01"), Some("2024-01-15")];
    let amts = [None, Some("0.50"), Some("1.00")];
    let rs = [None, Some(7.5)];
    let cs = [None, Some("x   ")];
    let schema = schema();
    let mut rows = Vec::new();
    let mut id = 0u64;
    for n in ns {
        for code in codes {
            for flag in flags {
                for d in dates {
                    for amt in amts {
                        for r in rs {
                            for c in cs {
                                id = (id + 1) % 13;
                                let bytes = crate::row_codec::encode_scalar_columns(
                                    &schema,
                                    &[
                                        Value::Vector(vec![0.0, 0.0]),
                                        n.map_or(Value::Null, Value::Integer),
                                        code.map_or(Value::Null, |s| Value::Text(s.to_string())),
                                        flag.map_or(Value::Null, Value::Bool),
                                        d.map_or(Value::Null, |s| Value::Date(day(s))),
                                        amt.map_or(Value::Null, |s| Value::Numeric(dec(s))),
                                        r.map_or(Value::Null, Value::Double),
                                        c.map_or(Value::Null, |s| Value::Text(s.to_string())),
                                    ],
                                )
                                .expect("encode");
                                rows.push((id, bytes));
                            }
                        }
                    }
                }
            }
        }
    }
    rows
}

fn matches_row(conj: &BoundConjunction, id: u64, bytes: &[u8]) -> bool {
    let schema = schema();
    let scanned = crate::row_codec::scan_scalar_columns(&schema, bytes).expect("scan");
    let group = BoundOrGroup::new(vec![conj.clone()]);
    group
        .matches(&scanned, id, &[], 0, &mut Vec::new())
        .unwrap_or(false)
}

const PREDICATES: &[&str] = &[
    "n = 5",
    "n = 6",
    "n > 5",
    "n > 4",
    "n >= 5",
    "n > 10",
    "n < 6",
    "n <= 5",
    "n BETWEEN 0 AND 20",
    "n BETWEEN 5 AND 5",
    "n >= 5 AND n <= 5",
    "n > 4 AND n < 7",
    "n IN (5, 6)",
    "n IN (4, 5, 6)",
    "n IS NULL",
    "n IS NOT NULL",
    "n <> 5",
    "5 < n",
    "n > '4'",
    "n = 1 OR n = 2",
    "NOT n > 5",
    "NOT n < 3",
    "code = 'a'",
    "code = 'b'",
    "code IN ('a', 'b')",
    "code IN ('a', 'b', 'c')",
    "code IS NULL",
    "code IS NOT NULL",
    "code > 'a'",
    "code LIKE 'a%'",
    "code <> 'a'",
    "flag",
    "NOT flag",
    "flag = true",
    "d >= '2023-01-01'",
    "d >= '2024-01-01' AND d < '2024-02-01'",
    "d < '2024-02-01'",
    "amt = 1.0",
    "amt = 1.00",
    "amt > 0.1",
    "amt >= 1",
    "r > 5",
    "r > 10",
    "c = 'x'",
    "id > 3",
    "id >= 4",
    "id <= 5",
    "n > 4 AND code = 'a'",
    "n > 5 AND code IN ('a', 'b') AND flag",
    "n IS NOT NULL AND code IS NOT NULL",
];

/// 偽陽性が無いこと（INDEX-11）。含意と判定した全組について、グリッドの全行で Q が TRUE なら P も TRUE。
#[test]
fn index11_implication_has_no_false_positives_over_the_grid() {
    let rows = grid();
    let bound: Vec<(&str, BoundConjunction)> = PREDICATES.iter().map(|t| (*t, bind(t))).collect();
    let q_truth: Vec<Vec<bool>> = bound
        .iter()
        .map(|(_, c)| rows.iter().map(|(id, b)| matches_row(c, *id, b)).collect())
        .collect();
    let mut positives = 0usize;
    for (qi, (qt, qc)) in bound.iter().enumerate() {
        for (pt, pc) in &bound {
            let implied = query_implies_partial_predicate(
                &qc.metadata_filters,
                &qc.expr_filters,
                &qc.or_groups,
                pc,
                &schema(),
            );
            if !implied {
                continue;
            }
            positives += 1;
            for (ri, (id, bytes)) in rows.iter().enumerate() {
                if q_truth[qi][ri] {
                    assert!(
                        matches_row(pc, *id, bytes),
                        "false positive: `{qt}` claimed to imply `{pt}`"
                    );
                }
            }
        }
    }
    // 自己含意だけでも述語数ぶんは成立する。判定が空振りしていないことを確かめる。
    assert!(positives > PREDICATES.len(), "positives = {positives}");
}

#[test]
fn index11_explicit_positive_cases() {
    for (q, p) in [
        ("n > 5", "n > 5"),
        ("n = 5", "n > 3"),
        ("n > 10", "n > 5"),
        ("n > 5", "n >= 5"),
        ("n >= 5 AND n <= 8", "n BETWEEN 0 AND 20"),
        ("n > 5 AND n < 10", "n BETWEEN 0 AND 20"),
        ("n = 5", "n IS NOT NULL"),
        ("n > 5", "n IS NOT NULL"),
        ("n IS NULL", "n IS NULL"),
        ("n IS NOT NULL", "n IS NOT NULL"),
        ("code = 'a'", "code IN ('a', 'b')"),
        ("code IN ('a', 'b')", "code IN ('a', 'b', 'c')"),
        ("code = 'a'", "code IS NOT NULL"),
        (
            "d >= '2024-01-01' AND d < '2024-02-01'",
            "d >= '2023-01-01'",
        ),
        ("amt = 1.0", "amt = 1.00"),
        ("flag", "flag"),
        ("flag", "flag IS NOT NULL"),
        ("n > 4 AND code = 'a'", "n > 3 AND code IN ('a', 'z')"),
        ("5 < n", "n > 3"),
        ("n > '10'", "n > 5"),
        ("n > 5 AND code = 'q' AND r > 1", "n >= 5"),
        ("n >= 5 AND n <= 5", "n = 5"),
        ("n = 5 AND (code = 'a' OR code = 'b')", "n = 5"),
        ("id > 10", "id > 3"),
        ("n > 5", "NOT n < 3"),
        // 構造的に同一の項（認識外の形も同じ項があれば含意）。
        ("n <> 5 AND code = 'a'", "n <> 5"),
        ("code LIKE 'a%'", "code LIKE 'a%'"),
        ("code = 'a' OR n > 5", "code = 'a' OR n > 5"),
        ("n = 1 OR n = 2", "n = 1 OR n = 2"),
        ("code > 'b'", "code > 'b'"),
    ] {
        assert!(implies(q, p), "expected `{q}` => `{p}`");
    }
}

#[test]
fn index11_explicit_negative_cases_stay_conservative() {
    for (q, p) in [
        ("n > 5", "n > 6"),
        ("n >= 5", "n > 5"),
        // 整数性は使わない。
        ("n > 4", "n >= 5"),
        ("n IS NOT NULL", "n > 0"),
        ("n IS NULL", "n IS NOT NULL"),
        ("n IS NOT NULL", "n IS NULL"),
        ("n IS NULL", "n > 0"),
        ("code IN ('a', 'b')", "code = 'a'"),
        ("code = 'a'", "code = 'b'"),
        ("n > 5", "code = 'a'"),
        // Q が空に近い・認識外。
        ("code LIKE 'a%'", "code = 'a'"),
        ("n = 1 OR n = 2", "n < 10"),
        ("n > 5", "n = 1 OR n = 2"),
        ("n > 5", "n <> 5"),
        ("flag", "NOT flag"),
        // 浮動小数・bpchar・TEXT 範囲・式は対象外。
        ("r > 10", "r > 5"),
        ("c = 'x'", "c = 'y'"),
        ("code > 'b'", "code > 'a'"),
        ("n + 1 > 3", "n > 2"),
        ("n > 5", "n + 1 > 3"),
        ("id > 3", "id > 10"),
        // P の一部だけ成立。
        ("n > 5", "n > 3 AND code = 'a'"),
        // 日付の境界。
        ("d >= '2024-01-01'", "d > '2024-01-01'"),
    ] {
        assert!(!implies(q, p), "expected `{q}` =/=> `{p}`");
    }
}

/// 述語項が 0 個の P は含意しない（保守側）。
#[test]
fn index11_empty_index_conjunction_never_implied() {
    let empty = BoundConjunction::new(Vec::new(), Vec::new(), Vec::new());
    let q = bind("n > 5");
    assert!(!query_implies_partial_predicate(
        &q.metadata_filters,
        &q.expr_filters,
        &q.or_groups,
        &empty,
        &schema(),
    ));
}

/// 作業予算が尽きたら（本来は含意する形でも）`false`。
#[test]
fn index11_exhausted_budget_is_not_implied() {
    let q = "n > 5 AND code IN ('a', 'b', 'c')";
    let p = "n > 3 AND code IN ('a', 'b', 'c', 'd')";
    assert!(implies_with(q, p, MAX_IMPLICATION_STEPS));
    assert!(!implies_with(q, p, 3));
    assert!(!implies_with(q, p, 0));
}

/// 大きな `IN` 同士でも panic せず、既定予算の内で決定的に判定する。
#[test]
fn index11_large_in_lists_are_bounded() {
    let list = |range: std::ops::Range<u32>| {
        range
            .map(|i| format!("'v{i:03}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let q = format!("code IN ({})", list(0..200));
    let p_superset = format!("code IN ({})", list(0..256));
    let p_disjoint = format!("code IN ({})", list(200..256));
    assert!(implies(&q, &p_superset));
    assert!(!implies(&q, &p_disjoint));
}

/// 索引述語のアクセサ経由（#1656 が使う形）でも同じ判定になる。
#[test]
fn index11_bound_index_predicate_exposes_its_conjunction() {
    let preds = crate::sql::allowlist::parse_index_predicate_text("n > 3").expect("parse");
    let bound = crate::sql::index_predicate::bind_index_predicate(&preds, &schema()).expect("bind");
    let p = bound.conjunction().expect("single branch");
    let q = bind("n = 5");
    assert!(query_implies_partial_predicate(
        &q.metadata_filters,
        &q.expr_filters,
        &q.or_groups,
        p,
        &schema(),
    ));
}
