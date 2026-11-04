//! `benches/harness/dot_kernel.rs`（Issue #365。`isa.rs` dot カーネルの複数
//! アキュムレータ化マイクロベンチ）の回帰テスト。
//!
//! `dot_kernel_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/hybrid_latency_accept.rs` と同様、実測タイマー・env に依存しない
//! 時間非依存の契約のみを `#[path]` で取り込み `cargo test`〔`make ci` 対象〕で
//! 検証する）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::dot_kernel::{
    check_bit_identical, check_l2_matches_scalar_reference, check_matches_scalar_reference,
    classify_change, generate_corpus, generate_query, min_of_samples, ns_per_dot, parse_l2_ab_env,
    parse_tail_ab_env, refuse_under_github_actions, relative_band, render_l2_ab_line,
    render_l2_diagnostic_line, render_line, render_tail_ab_line, render_tail_ab_reference_line,
    rows_for, speedup_ratio, ChangeClass, DotKernelError, L2AbMode, L2AbRow, TailAbMode,
    WorkingSet, ARENA_SCALE_ROWS, MAX_CORPUS_ROWS_GUARD, MAX_DIM_GUARD, TAIL_AB_DIMS,
};
use std::time::Duration;

// --- L2 A/B（Issue #1984・SQL-33 ポインタ） ---

#[test]
fn parse_l2_ab_env_accepts_fixed_vocabulary() {
    assert_eq!(parse_l2_ab_env(None).unwrap(), L2AbMode::Off);
    assert_eq!(parse_l2_ab_env(Some("0")).unwrap(), L2AbMode::Off);
    assert_eq!(parse_l2_ab_env(Some("1")).unwrap(), L2AbMode::On);
}

#[test]
fn parse_l2_ab_env_rejects_other_values() {
    for bogus in ["", "true", "yes", "2", " 1"] {
        let err = parse_l2_ab_env(Some(bogus)).unwrap_err();
        assert!(
            matches!(err, DotKernelError::InvalidEnv { .. }),
            "expected InvalidEnv for {bogus:?}, got {err:?}"
        );
    }
}

#[test]
fn check_l2_matches_scalar_reference_accepts_within_tolerance() {
    check_l2_matches_scalar_reference(1000.0, 1000.0).unwrap();
    check_l2_matches_scalar_reference(1000.009, 1000.0).unwrap();
}

#[test]
fn check_l2_matches_scalar_reference_rejects_out_of_tolerance() {
    let err = check_l2_matches_scalar_reference(1000.5, 1000.0).unwrap_err();
    assert!(matches!(err, DotKernelError::L2ToleranceExceeded { .. }));
    assert!(err.to_string().contains("l2 result outside tolerance"));
    assert!(err.to_string().contains("1000.5"));
}

#[test]
fn check_l2_matches_scalar_reference_rejects_non_finite() {
    for (a, e) in [
        (f32::NAN, 1.0),
        (f32::INFINITY, 1.0),
        (1.0, f32::NAN),
        (1.0, f32::INFINITY),
    ] {
        assert_eq!(
            check_l2_matches_scalar_reference(a, e).unwrap_err(),
            DotKernelError::NonFiniteResult
        );
    }
}

#[test]
fn l2_ab_lines_carry_fields_and_do_not_collide_with_chip_parsers() {
    let line = render_l2_ab_line(&L2AbRow {
        working_set: WorkingSet::CacheResident,
        dim: 768,
        rows: 21,
        dot_min: Duration::from_micros(1500),
        dot_median: Duration::from_micros(1600),
        l2_min: Duration::from_micros(3000),
        l2_median: Duration::from_micros(3200),
        ratio_min: 2.0,
        ratio_median: 2.0,
    });
    for needle in [
        "dot_kernel: l2_ab ",
        "working_set=cache_resident",
        "dim=768",
        "rows=21",
        "dot_min_ms=1.500",
        "l2_median_ms=3.200",
        "ratio_min=2.0000",
        "ratio_median=2.0000",
    ] {
        assert!(line.contains(needle), "missing {needle:?} in {line}");
    }
    let diag = render_l2_diagnostic_line(768, 21, 1.0, ChangeClass::Neutral);
    assert!(diag.starts_with("dot_kernel: l2_diagnostic_ab dim=768 rows=21 "));
    assert!(diag.contains("simd_vs_scalar_ratio=1.000"));
    for l in [&line, &diag] {
        assert!(!l.starts_with("dot_kernel: label=current "));
        assert!(!l.starts_with("dot_kernel: diagnostic_ab "));
        assert!(!l.starts_with("dot_kernel: block4_ab"));
        assert!(harness::chip::parse_dot_kernel_line(l).is_none());
        assert!(harness::chip::parse_dot_kernel_diag_line(l).is_none());
    }
}

// --- refuse_under_github_actions ---

#[test]
fn refuse_under_github_actions_rejects_when_true() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, DotKernelError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_false() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- rows_for ---

#[test]
fn rows_for_cache_resident_is_deterministic_and_positive() {
    let a = rows_for(WorkingSet::CacheResident, 768).expect("rows ok");
    let b = rows_for(WorkingSet::CacheResident, 768).expect("rows ok");
    assert_eq!(a, b);
    assert!(a >= 1);
}

#[test]
fn rows_for_cache_resident_shrinks_as_dim_grows() {
    let small_dim = rows_for(WorkingSet::CacheResident, 128).expect("rows ok");
    let large_dim = rows_for(WorkingSet::CacheResident, 1536).expect("rows ok");
    assert!(
        large_dim <= small_dim,
        "larger dim must not need more rows to fill the same byte budget: {large_dim} vs {small_dim}"
    );
}

#[test]
fn rows_for_cache_resident_has_lower_bound_of_one() {
    // 極端に大きい dim でもゼロ行にはならない（最低 1 行保証）。
    let rows = rows_for(WorkingSet::CacheResident, 10_000_000).expect("rows ok");
    assert_eq!(rows, 1);
}

#[test]
fn rows_for_arena_scale_is_fixed() {
    let rows = rows_for(WorkingSet::ArenaScale, 128).expect("rows ok");
    assert_eq!(rows, ARENA_SCALE_ROWS);
}

// --- generate_corpus ---

#[test]
fn generate_corpus_is_deterministic_for_same_seed() {
    let a = generate_corpus(1, 8, 16).expect("corpus ok");
    let b = generate_corpus(1, 8, 16).expect("corpus ok");
    assert_eq!(a, b);
    assert_eq!(a.len(), 8 * 16);
}

#[test]
fn generate_corpus_differs_across_seeds() {
    let a = generate_corpus(1, 8, 16).expect("corpus ok");
    let b = generate_corpus(2, 8, 16).expect("corpus ok");
    assert_ne!(a, b, "異なるシードから同一コーパスが生成された");
}

#[test]
fn generate_corpus_rejects_rows_beyond_guard() {
    let err = generate_corpus(1, 8, MAX_CORPUS_ROWS_GUARD + 1).unwrap_err();
    assert_eq!(err, DotKernelError::CorpusTooLarge);
}

// --- generate_query ---

#[test]
fn generate_query_is_deterministic_and_differs_from_corpus_seed() {
    let q1 = generate_query(1, 8);
    let q2 = generate_query(1, 8);
    assert_eq!(q1, q2);
    assert_eq!(q1.len(), 8);

    let corpus_row = generate_corpus(1, 8, 1).expect("corpus ok");
    assert_ne!(
        q1, corpus_row,
        "クエリ生成がコーパス生成と同一の乱数系列を共有している"
    );
}

// --- ns_per_dot ---

#[test]
fn ns_per_dot_rejects_zero_dots() {
    let err = ns_per_dot(Duration::from_millis(10), 0).unwrap_err();
    assert_eq!(err, DotKernelError::ZeroDots);
}

#[test]
fn ns_per_dot_computes_expected_value() {
    let ns = ns_per_dot(Duration::from_millis(1), 1_000_000).expect("ns ok");
    assert!((ns - 1.0).abs() < 1e-6, "expected ~1.0 ns/dot, got {ns}");
}

// --- speedup_ratio / classify_change ---

#[test]
fn speedup_ratio_computes_candidate_over_baseline() {
    assert!((speedup_ratio(100.0, 50.0) - 0.5).abs() < 1e-9);
    assert!((speedup_ratio(100.0, 150.0) - 1.5).abs() < 1e-9);
}

#[test]
fn classify_change_boundaries() {
    let noise_band = 0.05;
    assert_eq!(
        classify_change(0.95, noise_band),
        ChangeClass::Improved,
        "ratio exactly at the improved boundary must classify as Improved"
    );
    assert_eq!(classify_change(0.90, noise_band), ChangeClass::Improved);
    assert_eq!(classify_change(1.0, noise_band), ChangeClass::Neutral);
    assert_eq!(
        classify_change(1.05, noise_band),
        ChangeClass::Regressed,
        "ratio exactly at the regressed boundary must classify as Regressed"
    );
    assert_eq!(classify_change(1.10, noise_band), ChangeClass::Regressed);
    // ノイズ帯の内側（境界未満・未満）は Neutral。
    assert_eq!(classify_change(0.96, noise_band), ChangeClass::Neutral);
    assert_eq!(classify_change(1.04, noise_band), ChangeClass::Neutral);
}

// --- render_line ---

#[test]
fn render_line_includes_all_fields() {
    let line = render_line(
        "current",
        WorkingSet::CacheResident,
        768,
        20,
        Duration::from_millis(5),
        123.4,
    );
    assert!(line.contains("label=current"));
    assert!(line.contains("working_set=cache_resident"));
    assert!(line.contains("dim=768"));
    assert!(line.contains("rows=20"));
    assert!(line.contains("ns_per_dot=123.40"));
}

#[test]
fn render_line_arena_scale_label() {
    let line = render_line(
        "current",
        WorkingSet::ArenaScale,
        128,
        25_000,
        Duration::from_millis(5),
        1.0,
    );
    assert!(line.contains("working_set=arena_scale"));
}

// --- check_matches_scalar_reference ---

#[test]
fn check_matches_scalar_reference_accepts_exact_match() {
    assert!(check_matches_scalar_reference(1.0, 1.0, 1.0).is_ok());
}

#[test]
fn check_matches_scalar_reference_accepts_within_tolerance() {
    assert!(check_matches_scalar_reference(100.00005, 100.0, 100.0).is_ok());
}

#[test]
fn check_matches_scalar_reference_rejects_beyond_tolerance() {
    let err = check_matches_scalar_reference(2.0, 1.0, 1.0).unwrap_err();
    assert_eq!(
        err,
        DotKernelError::ToleranceExceeded {
            actual: 2.0,
            expected: 1.0
        }
    );
}

#[test]
fn check_matches_scalar_reference_rejects_non_finite() {
    let err = check_matches_scalar_reference(f32::NAN, 1.0, 1.0).unwrap_err();
    assert_eq!(err, DotKernelError::NonFiniteResult);
}

// --- Issue #529: tail A/B ---

#[test]
fn tail_ab_dims_are_within_dim_guard() {
    for &dim in &TAIL_AB_DIMS {
        assert!(dim >= 1);
        assert!(dim <= MAX_DIM_GUARD);
    }
}

#[test]
fn tail_ab_dims_match_plan_selection() {
    // dim 選定の意味（端数長の異なる境界）: 100（中程度）・129（最小端数）・
    // 768（端数ゼロ・定数上乗せコストの測定点）。この 3 点は
    // `docs/design/dot-kernel-branchless-tail.md`「#529 への申し送り」節の
    // 指定と一致していなければならない。
    assert_eq!(TAIL_AB_DIMS, [100, 129, 768]);
}

#[test]
fn parse_tail_ab_env_unset_is_off() {
    assert_eq!(parse_tail_ab_env(None).unwrap(), TailAbMode::Off);
}

#[test]
fn parse_tail_ab_env_zero_is_off() {
    assert_eq!(parse_tail_ab_env(Some("0")).unwrap(), TailAbMode::Off);
}

#[test]
fn parse_tail_ab_env_one_is_on() {
    assert_eq!(parse_tail_ab_env(Some("1")).unwrap(), TailAbMode::On);
}

#[test]
fn parse_tail_ab_env_rejects_empty_string() {
    let err = parse_tail_ab_env(Some("")).unwrap_err();
    assert!(matches!(err, DotKernelError::InvalidEnv { .. }));
}

#[test]
fn parse_tail_ab_env_rejects_unrecognized_values() {
    for bogus in ["true", "ON", "yes", "2", " 1", "1 "] {
        let err = parse_tail_ab_env(Some(bogus)).unwrap_err();
        assert!(
            matches!(err, DotKernelError::InvalidEnv { .. }),
            "expected InvalidEnv for {bogus:?}, got {err:?}"
        );
    }
}

#[test]
fn min_of_samples_returns_the_minimum() {
    let samples = [
        Duration::from_micros(30),
        Duration::from_micros(10),
        Duration::from_micros(20),
    ];
    assert_eq!(min_of_samples(&samples).unwrap(), Duration::from_micros(10));
}

#[test]
fn min_of_samples_rejects_empty_input() {
    let err = min_of_samples(&[]).unwrap_err();
    assert_eq!(err, DotKernelError::EmptySamples);
}

#[test]
fn relative_band_computes_example_from_policy_doc() {
    // `docs/design/benchmark-judgement-policy.md` §4 の例（1000〜1050 →
    // reference_band = 5.0%）と同じ値で検証する。
    let band = relative_band(&[1000.0, 1020.0, 1050.0]).unwrap();
    assert!((band - 0.05).abs() < 1e-9, "expected 0.05, got {band}");
}

#[test]
fn relative_band_rejects_empty_input() {
    let err = relative_band(&[]).unwrap_err();
    assert_eq!(err, DotKernelError::EmptySamples);
}

#[test]
fn relative_band_rejects_non_finite_values() {
    let err = relative_band(&[1.0, f64::NAN, 2.0]).unwrap_err();
    assert_eq!(err, DotKernelError::NonFiniteOrNonPositiveBand);

    let err = relative_band(&[1.0, f64::INFINITY]).unwrap_err();
    assert_eq!(err, DotKernelError::NonFiniteOrNonPositiveBand);
}

#[test]
fn relative_band_rejects_non_positive_minimum() {
    let err = relative_band(&[0.0, 5.0]).unwrap_err();
    assert_eq!(err, DotKernelError::NonFiniteOrNonPositiveBand);

    let err = relative_band(&[-1.0, 5.0]).unwrap_err();
    assert_eq!(err, DotKernelError::NonFiniteOrNonPositiveBand);
}

#[test]
fn check_bit_identical_accepts_equal_bits() {
    assert!(check_bit_identical(768, 0, 1.5, 1.5).is_ok());
    // 符号付きゼロは値としては等しいがビットパターンが異なるため、
    // `to_bits()` 一致契約では区別されるべき（順序保存契約のビット同一性は
    // 値の等価性ではなくビットパターンの一致を要求する）。
    assert!(check_bit_identical(768, 0, 0.0, -0.0).is_err());
}

#[test]
fn check_bit_identical_rejects_differing_bits() {
    let err = check_bit_identical(129, 3, 1.0, 1.0000001).unwrap_err();
    assert!(matches!(
        err,
        DotKernelError::BitMismatch {
            dim: 129,
            row: 3,
            ..
        }
    ));
}

#[test]
fn render_tail_ab_line_uses_distinct_prefix_from_current_label() {
    let line = render_tail_ab_line(
        "scalar_tail",
        129,
        25_000,
        Duration::from_micros(10),
        Duration::from_micros(12),
        1.0,
        1.0,
        ChangeClass::Neutral,
    );
    assert!(line.starts_with("dot_kernel: tail_ab "));
    // `chip.rs::parse_dot_kernel_line` は `dot_kernel: label=current ` prefix
    // のみを拾う。tail A/B 行がこの prefix と衝突しないことを固定する。
    assert!(!line.contains("label=current"));
    assert!(line.contains("label=scalar_tail"));
    assert!(line.contains("dim=129"));
}

#[test]
fn render_tail_ab_reference_line_uses_distinct_prefix() {
    let line = render_tail_ab_reference_line(
        768,
        25_000,
        Duration::from_micros(5),
        Duration::from_micros(6),
    );
    assert!(line.starts_with("dot_kernel: tail_ab_ref "));
    assert!(!line.contains("label=current"));
    assert!(line.contains("dim=768"));
}
