//! `benches/harness/relational_p95.rs`（Issue #1204。述語・順序・結合の p95 計測。
//! ポインタ: SQL-24・SQL-25・SQL-28・RLS-10）の回帰テスト。
//!
//! `relational_p95_bench.rs` は時間依存のためここでは実行せず、実測タイマーに依存しない
//! 契約（env の fail-closed パース・SQL 文の構造・fixture 値・ラウンド統計・出力行）のみを
//! `#[path]` で取り込み `cargo test`（`make ci` 対象）で検証する
//! （`tests/parse_bind_bench_accept.rs` と同方針）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use std::time::Duration;

use harness::relational_p95::*;
use harness::sql_c1::vector_literal;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn parse_rounds_boundaries() {
    assert_eq!(parse_rounds(None), Ok(DEFAULT_ROUNDS));
    assert_eq!(parse_rounds(Some("  ")), Ok(DEFAULT_ROUNDS));
    assert_eq!(parse_rounds(Some("5")), Ok(5));
    assert_eq!(parse_rounds(Some("50")), Ok(50));
    for bad in ["4", "51", "abc", "-1", "5.5"] {
        assert!(parse_rounds(Some(bad)).is_err(), "{bad}");
    }
}

#[test]
fn parse_group_exact_match_only() {
    assert_eq!(parse_group(None), Ok(Group::All));
    assert_eq!(parse_group(Some("predicate")), Ok(Group::Predicate));
    assert_eq!(parse_group(Some("order_by")), Ok(Group::OrderBy));
    assert_eq!(parse_group(Some("join")), Ok(Group::Join));
    for bad in ["Predicate", "pred", "join;", "all,join"] {
        assert!(parse_group(Some(bad)).is_err(), "{bad}");
    }
    assert!(Group::All.includes(Group::Join));
    assert!(!Group::Join.includes(Group::OrderBy));
}

#[test]
fn parse_rows_scale_bounds() {
    assert_eq!(parse_rows_scale(None), Ok(DEFAULT_ROWS));
    assert_eq!(parse_rows_scale(Some("1000")), Ok(1_000));
    assert_eq!(parse_rows_scale(Some("100000")), Ok(100_000));
    for bad in ["999", "100001", "x", "-5"] {
        assert!(parse_rows_scale(Some(bad)).is_err(), "{bad}");
    }
    assert_eq!(scale_label(DEFAULT_ROWS), "full");
    assert_eq!(scale_label(5_000), "reduced");
}

#[test]
fn predicate_statement_structure() {
    let lit = vector_literal(&[0.5, 1.0]).expect("literal");
    let stmts = predicate_statements("docs", &lit).expect("statements");
    assert_eq!(stmts.len(), 3);
    assert_eq!(stmts[0].0, REFERENCE_ARM);
    let or2 = &stmts[1].1;
    assert_eq!(or2.matches(" OR ").count(), 1);
    let in8 = &stmts[2].1;
    let list = in8
        .split("IN (")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .expect("in list");
    assert_eq!(list.split(',').count(), 8);
    for (_, sql) in &stmts {
        let last = sql.split_whitespace().last().expect("token");
        assert_eq!(last, PRED_LIMIT.to_string());
        assert!(sql.contains("LIMIT"));
        assert_eq!(sql.matches('\'').count() % 2, 0);
    }
}

#[test]
fn order_and_join_statements_have_limit() {
    for (_, sql) in order_by_statements("docs").expect("order") {
        assert!(sql.ends_with(&format!("LIMIT {WIDE_LIMIT}")));
    }
    let (label, sql) = join_statement("documents", "authors").expect("join");
    assert_eq!(label, "join_inner");
    assert!(sql.contains("JOIN authors ON documents.author_id = authors.id"));
    assert!(sql.ends_with(&format!("LIMIT {WIDE_LIMIT}")));
}

#[test]
fn identifier_and_token_validation_rejects_injection() {
    let lit = vector_literal(&[1.0]).expect("literal");
    for bad in ["a b", "a;b", "a'b", "", "1a", "a-b"] {
        assert!(predicate_statements(bad, &lit).is_err(), "{bad}");
        assert!(order_by_statements(bad).is_err(), "{bad}");
        assert!(join_statement(bad, "authors").is_err(), "{bad}");
        assert!(join_statement("documents", bad).is_err(), "{bad}");
    }
    assert!(is_valid_value_token("l0"));
    for bad in ["", "a b", "a'b", "a;b"] {
        assert!(!is_valid_value_token(bad), "{bad}");
    }
}

#[test]
fn lang_selectivity_is_fixed() {
    let n = 1_600u64;
    let l0 = (0..n).filter(|&i| lang_for_id(i) == "l0").count() as u64;
    let first8 = (0..n)
        .filter(|&i| lang_in_first_n(lang_for_id(i), 8))
        .count() as u64;
    assert_eq!(l0, n / 16);
    assert_eq!(first8, n / 2);
}

#[test]
fn qty_is_deterministic_and_not_id_ordered() {
    assert_eq!(qty_for_id(7), qty_for_id(7));
    let values: Vec<i64> = (0..200).map(qty_for_id).collect();
    assert!(!is_sorted_by_direction(&values, false));
    assert!(!is_sorted_by_direction(&values, true));
    assert!(values.iter().all(|v| (0..1_000_000).contains(v)));
    assert_eq!(author_id_for_doc(25, 10), 5);
}

#[test]
fn sorted_direction_helper() {
    assert!(is_sorted_by_direction(&[1, 1, 2], false));
    assert!(!is_sorted_by_direction(&[2, 1], false));
    assert!(is_sorted_by_direction(&[3, 2, 2], true));
    assert!(is_sorted_by_direction(&[], false));
}

#[test]
fn summarize_rounds_values() {
    let s = summarize_rounds(&[ms(12), ms(10), ms(20), ms(11), ms(15)]).expect("summary");
    assert_eq!(s.min, ms(10));
    assert_eq!(s.median, ms(12));
    assert_eq!(s.max, ms(20));
    assert!((s.run_to_run_band_pct - 100.0).abs() < 1e-9);
    let even = summarize_rounds(&[ms(10), ms(20)]).expect("summary");
    assert_eq!(even.median, ms(15));
    assert!(summarize_rounds(&[]).is_err());
    assert!(summarize_rounds(&[Duration::ZERO, ms(1)]).is_err());
}

#[test]
fn ratio_vs_reference_rejects_zero_denominator() {
    let r = ratio_vs_reference(ms(30), ms(10)).expect("ratio");
    assert!((r - 3.0).abs() < 1e-9);
    assert!(ratio_vs_reference(ms(1), Duration::ZERO).is_err());
}

#[test]
fn round_p95_picks_nearest_rank() {
    let samples: Vec<Duration> = (1..=100).map(ms).collect();
    assert_eq!(round_p95(&samples), Ok(ms(95)));
    assert!(round_p95(&[]).is_err());
}

#[test]
fn rotate_arms_permutes_and_shifts_start() {
    let mut starts = Vec::new();
    for round in 0..3 {
        let order = rotate_arms(round, 3);
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2]);
        starts.push(order[0]);
    }
    assert_eq!(starts, vec![0, 1, 2]);
    assert!(rotate_arms(0, 0).is_empty());
}

#[test]
fn interleave_places_reference_before_each_candidate() {
    // 参照 = 0、候補 = 1, 2。各ラウンドで ref/cand/ref/cand の並びになり、候補の開始位置が回転する。
    assert_eq!(interleave_with_reference(0, 3, Some(0)), vec![0, 1, 0, 2]);
    assert_eq!(interleave_with_reference(1, 3, Some(0)), vec![0, 2, 0, 1]);
    assert_eq!(interleave_with_reference(2, 3, Some(0)), vec![0, 1, 0, 2]);
    // 参照 arm が無ければ通常の輪番へ退避する。
    assert_eq!(interleave_with_reference(1, 3, None), rotate_arms(1, 3));
    assert_eq!(interleave_with_reference(0, 1, Some(0)), vec![0]);
}

#[test]
fn rendered_lines_have_keys_and_no_tenant() {
    let s = summarize_rounds(&[ms(10), ms(12)]).expect("summary");
    let round = render_round_line("predicate", "pred_or2", 1, ms(10), ms(5), "0.1 0.1 0.1");
    assert!(round.contains("p95=") && round.contains("round=1"));
    let sum = render_summary_line(
        "predicate",
        "pred_or2",
        5_000,
        scale_label(5_000),
        &s,
        Some(1.5),
    );
    assert!(sum.contains("min_of_n=") && sum.contains("scale=reduced"));
    assert!(sum.contains("ratio_vs_pred_eq=1.500"));
    for line in [&round, &sum] {
        assert!(!line.contains("tenant"));
        assert!(!line.contains("SELECT"));
    }
}

#[test]
fn threshold_line_states_not_evaluated_when_shared() {
    assert!(render_threshold_line(false).contains("not evaluated"));
    assert!(!render_threshold_line(true).contains("not evaluated"));
}

#[test]
fn github_actions_is_refused() {
    assert_eq!(
        refuse_under_github_actions(true),
        Err(RelationalP95Error::RefusedUnderGithubActions)
    );
    assert_eq!(refuse_under_github_actions(false), Ok(()));
}

#[test]
fn exact_id_set_detects_duplicate_plus_missing() {
    let expected = [0u64, 1, 2, 3];
    assert!(is_exact_id_set(&[3, 1, 0, 2], &expected));
    // 件数は同じでも、重複（1 が 2 回）と欠落（3 が無い）が同時に起きれば拒否する。
    assert!(!is_exact_id_set(&[0, 1, 1, 2], &expected));
    assert!(!is_exact_id_set(&[0, 1, 2], &expected));
    assert!(!is_exact_id_set(&[0, 1, 2, 3, 3], &expected));
}

#[test]
fn expected_order_prefixes_follow_fixture() {
    let single = expected_order_single(2_000, 100);
    assert_eq!(single.len(), 100);
    assert!(is_sorted_by_direction(&single, false));
    let brute_min = (0..2_000u64).map(qty_for_id).min().expect("non-empty");
    assert_eq!(single.first().copied(), Some(brute_min));

    let multi = expected_order_multi(2_000, 100);
    assert_eq!(multi.len(), 100);
    // 先頭は最小の lang（文字列順で "l0"）内の最大 qty。
    let best = (0..2_000u64)
        .filter(|id| lang_for_id(*id) == "l0")
        .map(qty_for_id)
        .max()
        .expect("l0 rows");
    assert_eq!(multi.first().copied(), Some(("l0", best)));
    for w in multi.windows(2) {
        assert!(w[0].0 < w[1].0 || (w[0].0 == w[1].0 && w[0].1 >= w[1].1));
    }
}
