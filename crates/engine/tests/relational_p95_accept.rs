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
    // 結合の規模は実際の結合行数で決まる（docs 行数が規定でも結合が縮小なら reduced）。
    assert_eq!(join_scale_label(JOIN_ROWS), "full");
    assert_eq!(join_scale_label(1_000), "reduced");
}

#[test]
fn pair_ratio_uses_reference_of_the_min_pair() {
    // 候補 min（8ms）のペアの参照（4ms）で割る。別ペアの参照 min（2ms）は使わない。
    let pairs = [(ms(10), ms(2)), (ms(8), ms(4)), (ms(9), ms(3))];
    let (ratio, reference) = pair_ratio(&pairs).expect("ratio");
    assert_eq!(reference, ms(4));
    assert!((ratio - 2.0).abs() < 1e-9);
    assert!(pair_ratio(&[]).is_err());
}

#[test]
fn predicate_statement_structure() {
    let lit = vector_literal(&[0.5, 1.0]).expect("literal");
    let stmts = predicate_statements("docs", &lit).expect("statements");
    assert_eq!(stmts.len(), 5);
    assert_eq!(stmts[0].0, REFERENCE_ARM);
    let by_label = |label: &str| -> &str {
        stmts
            .iter()
            .find(|(l, _)| *l == label)
            .map(|(_, s)| s.as_str())
            .expect("arm exists")
    };
    let in_list_len = |sql: &str| -> usize {
        sql.split("IN (")
            .nth(1)
            .and_then(|s| s.split(')').next())
            .expect("in list")
            .split(',')
            .count()
    };
    assert_eq!(by_label("pred_or2").matches(" OR ").count(), 1);
    // 診断用 arm: 同一リテラルの OR は参照 arm と同じ選択率、2 要素 IN は pred_or2 と同じ選択率。
    let same = by_label("pred_or_same");
    assert_eq!(same.matches(" OR ").count(), 1);
    let lits: Vec<&str> = same
        .split("WHERE ")
        .nth(1)
        .and_then(|s| s.split(" ORDER").next())
        .expect("where clause")
        .split(" OR ")
        .collect();
    assert_eq!(lits.len(), 2);
    assert_eq!(lits[0], lits[1]);
    assert_eq!(in_list_len(by_label("pred_in2")), 2);
    assert_eq!(in_list_len(by_label("pred_in8")), 8);
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
        Some((1.5, ms(8))),
    );
    assert!(sum.contains("min_of_n=") && sum.contains("scale=reduced"));
    assert!(sum.contains("ratio_vs_pred_eq=1.500 ref_paired=8.000ms"));
    for line in [&round, &sum] {
        assert!(!line.contains("tenant"));
        assert!(!line.contains("SELECT"));
    }
}

#[test]
fn threshold_line_states_not_evaluated_when_shared() {
    assert!(render_threshold_line(false, true).contains("not evaluated"));
    assert!(render_threshold_line(false, false).contains("not evaluated"));
    // 専有環境でも縮小規模なら閾値との比較を促さない。
    assert!(render_threshold_line(true, false).contains("reduced scale"));
    assert!(!render_threshold_line(true, true).contains("not evaluated"));
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
    let single = expected_order_single(&visible_doc_rows(2_000, 0), 100);
    assert_eq!(single.len(), 100);
    assert!(is_sorted_by_direction(&single, false));
    let brute_min = (0..2_000u64).map(qty_for_id).min().expect("non-empty");
    assert_eq!(single.first().copied(), Some(brute_min));

    let multi = expected_order_multi(&visible_doc_rows(2_000, 0), 100);
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

#[test]
fn topk_matches_detects_wrong_subset() {
    // 述語を満たす行が距離 0.1..0.5 で 5 行、k = 3。
    let ranked = [(10u64, 0.1), (11, 0.2), (12, 0.3), (13, 0.4), (14, 0.5)];
    assert!(topk_matches(&[10, 11, 12], &ranked, 3));
    // 同値境界（0.3 と 0.3 + eps 内）は id の入れ替わりを許す。
    let tied = [(1u64, 0.1), (2, 0.3), (3, 0.3), (4, 0.9)];
    assert!(topk_matches(&[1, 3], &tied, 2));
    // 上位に入らない行、取りこぼし、重複、件数不足、順序違い、述語外 id は拒否する。
    assert!(!topk_matches(&[10, 11, 14], &ranked, 3));
    assert!(!topk_matches(&[10, 12, 13], &ranked, 3));
    assert!(!topk_matches(&[10, 10, 11], &ranked, 3));
    assert!(!topk_matches(&[10, 11], &ranked, 3));
    assert!(!topk_matches(&[11, 10, 12], &ranked, 3));
    assert!(!topk_matches(&[10, 11, 99], &ranked, 3));
    assert!(topk_matches(&[], &[], 3));
}

#[test]
fn inner_product_distance_reference() {
    // 正規化しない内積のため、大きさの違うベクトルは距離が異なる（コサインなら同じ 0）。
    assert_eq!(inner_product_distance(&[1.0, 0.0], &[2.0, 0.0]), -2.0);
    assert_eq!(inner_product_distance(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    assert_eq!(inner_product_distance(&[3.0, 4.0], &[3.0, 4.0]), -25.0);
}

#[test]
fn visible_rows_include_sentinels_only_for_other_context() {
    assert_eq!(visible_doc_rows(10, 0).len(), 10);
    let both = visible_doc_rows(10, 4);
    assert_eq!(both.len(), 14);
    // sentinel は最小 qty（偶数 id）と最大 qty（奇数 id）を持ち、他テナント文脈の上位に現れる。
    assert_eq!(expected_order_single(&both, 2), vec![-1, -1]);
    assert_eq!(
        expected_order_multi(&both, 2),
        vec![("l0", 2_000_000), ("l0", 2_000_000)]
    );
    assert!(expected_order_single(&visible_doc_rows(10, 0), 2)
        .iter()
        .all(|q| *q >= 0));
}

#[test]
fn sentinel_scale_dominates_fixture() {
    let (max_dot, nsq) = (60.0, 256.0);
    let f = f64::from(sentinel_scale(max_dot, nsq));
    assert!(f >= 2.0);
    assert!(sentinel_dominates(f * nsq, max_dot));
    // fixture の最大内積が負・ゼロでも係数は 2 以上（クエリと同一以下にならない）。
    assert!(sentinel_scale(-5.0, nsq) >= 2.0);
    assert!(sentinel_scale(10.0, 0.0) >= 2.0);
    assert!(!sentinel_dominates(60.0, 60.0));
}

#[test]
fn expected_path_pins_or_arms_to_index_in_list() {
    // Issue #1305: 同じ列への等価 OR は束縛時に IN へ畳まれ索引経路になる。
    assert_eq!(expected_path("pred_eq"), ("index_equality", true));
    for arm in ["pred_or2", "pred_or_same", "pred_in2", "pred_in8"] {
        assert_eq!(expected_path(arm), ("index_in_list", true), "{arm}");
    }
    assert_eq!(expected_path("unknown_arm"), ("plain_scan", false));
}

// --- 述語経路の段別計測（Issue #1319。SQL-24・SQL-2 ポインタ） ---

fn us(n: u64) -> Duration {
    Duration::from_micros(n)
}

#[test]
fn quantile_nearest_rank_boundaries() {
    let odd = [us(5), us(1), us(3), us(2), us(4)];
    assert_eq!(quantile_nearest_rank(&odd, 0.5), Ok(us(3)));
    assert_eq!(quantile_nearest_rank(&odd, 0.25), Ok(us(2)));
    assert_eq!(quantile_nearest_rank(&odd, 0.75), Ok(us(4)));
    assert_eq!(quantile_nearest_rank(&odd, 1.0), Ok(us(5)));
    // 偶数長は nearest-rank（ceil(q * n) 番目）。
    let even = [us(4), us(1), us(3), us(2)];
    assert_eq!(quantile_nearest_rank(&even, 0.5), Ok(us(2)));
    assert_eq!(quantile_nearest_rank(&even, 0.75), Ok(us(3)));
    // 単一要素はどの q でもその値。
    assert_eq!(quantile_nearest_rank(&[us(7)], 0.25), Ok(us(7)));
    // 空・範囲外の q は拒否する。
    assert_eq!(
        quantile_nearest_rank(&[], 0.5),
        Err(RelationalP95Error::EmptySamples)
    );
    for bad in [0.0, -0.1, 1.01, f64::NAN] {
        assert_eq!(
            quantile_nearest_rank(&odd, bad),
            Err(RelationalP95Error::InvalidQuantile),
            "{bad}"
        );
    }
}

#[test]
fn summarize_stage_reports_median_quartiles_and_min() {
    let samples: Vec<Duration> = (1..=8).map(us).collect();
    let s = summarize_stage(&samples).expect("summary");
    assert_eq!((s.q1, s.median, s.q3, s.min), (us(2), us(4), us(6), us(1)));
    assert_eq!(s.samples, 8);
    assert!(summarize_stage(&[]).is_err());
}

#[test]
fn arm_values_match_statements() {
    let literal = vector_literal(&[0.5, 0.5]).expect("literal");
    let arms = predicate_statements("docs", &literal).expect("statements");
    let want = [
        ("pred_eq", 1usize, PredForm::Equality),
        ("pred_or2", 2, PredForm::OrBranches),
        ("pred_in2", 2, PredForm::InList),
        ("pred_or_same", 1, PredForm::OrBranches),
        ("pred_in8", 8, PredForm::InList),
    ];
    assert_eq!(arms.len(), want.len());
    for (label, distinct, form) in want {
        assert_eq!(arm_pred_form(label), Ok(form), "{label}");
        let values = arm_lang_values(label).expect("values");
        assert_eq!(values.len(), distinct, "{label}");
        // 値集合は SQL 文に現れる `'lN'` トークンと一致する（文と段別計測の取り違え検出）。
        let sql = &arms.iter().find(|(l, _)| *l == label).expect("arm").1;
        for v in &values {
            assert!(sql.contains(&format!("'{v}'")), "{label}: {v}");
        }
    }
    // 同一値 OR は分岐が 2 本、重複なし集合は 1 本。
    assert_eq!(arm_branch_values("pred_or_same").expect("b").len(), 2);
    assert!(arm_lang_values("nope").is_err());
    assert!(arm_pred_form("nope").is_err());
    assert!(expected_match_ids("nope", 10).is_err());
}

#[test]
fn expected_match_ids_follow_fixture_selectivity() {
    // lang は id % 16。1000 行なら 1/16・2/16・8/16 の選択率になる。
    let eq = expected_match_ids("pred_eq", 1_000).expect("ids");
    assert_eq!(eq.len(), 63);
    assert!(eq.iter().all(|id| id % 16 == 0));
    assert!(eq.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(expected_match_ids("pred_or_same", 1_000).expect("ids"), eq);
    let two = expected_match_ids("pred_or2", 1_000).expect("ids");
    assert_eq!(two, expected_match_ids("pred_in2", 1_000).expect("ids"));
    assert!(two.iter().all(|id| id % 16 < 2));
    assert_eq!(
        expected_match_ids("pred_in8", 1_000).expect("ids").len(),
        504
    );
}

#[test]
fn stage_diffs_use_checked_arithmetic() {
    assert_eq!(checked_stage_diff(us(10), us(4)), Some(us(6)));
    assert_eq!(checked_stage_diff(us(4), us(10)), None);
    assert_eq!(checked_residual(us(100), us(30), us(50)), Some(us(20)));
    assert_eq!(checked_residual(us(100), us(60), us(50)), None);
    let inverted = render_stage_diff_line("pred_or2", StagePath::Current, "residual_median", None);
    assert!(inverted.contains("residual_median=n/a"));
    let ok = render_stage_diff_line(
        "pred_or2",
        StagePath::PlainScanRef,
        "c_median",
        Some(us(12)),
    );
    assert!(ok.contains("path=plain_scan_ref") && ok.contains("c_median=12.000us"));
}

#[test]
fn stage_lines_have_keys_and_no_tenant() {
    let s = summarize_stage(&[us(3), us(1), us(2)]).expect("summary");
    let round = render_stage_round_line(
        "pred_in2",
        StagePath::Current,
        STAGE_IDX_SEARCH_SUBSET,
        2,
        us(5),
        "scanned_rows=10 k=10",
    );
    let sum = render_stage_summary_line(
        "pred_in2",
        StagePath::PlainScanRef,
        STAGE_PLAIN_COPY,
        &s,
        "matched=3 copied_bytes=9",
    );
    assert!(round.contains("stage=idx_search_subset") && round.contains("round=2"));
    assert!(round.contains("path=current") && round.contains("scanned_rows=10"));
    for key in [
        "median=",
        "q1=",
        "q3=",
        "min=",
        "samples=3",
        "path=plain_scan_ref",
    ] {
        assert!(sum.contains(key), "{key}");
    }
    for line in [&round, &sum] {
        assert!(!line.contains("tenant"));
        assert!(!line.contains("SELECT"));
    }
    assert_eq!(StagePath::Current.label(), "current");
    assert_eq!(StagePath::PlainScanRef.label(), "plain_scan_ref");
}
