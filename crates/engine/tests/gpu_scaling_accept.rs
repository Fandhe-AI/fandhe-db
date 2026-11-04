//! `benches/harness/gpu_scaling.rs`（GPU バッチ検索 vs CPU-SIMD の規模別実測
//! ベンチ。手動専用・`benches/gpu_scaling_bench.rs`）の時間非依存な回帰テスト。
//!
//! GPU デバイスにも `engine` クレートの内部型にも依存しない純関数
//! （env パース・出力整形・同点許容つき不一致検知）のみを対象とする
//! （`tests/bench_engine_accept.rs`・`tests/knn_profile_accept.rs` と同一方針）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::gpu_scaling::{
    count_boundary_tolerant_mismatches, format_i8_unavailable_line, format_metric_line,
    format_skip_line, format_unavailable_line, full_readback_bytes_estimate, mean_recall_at_k,
    parse_batches, parse_dims, parse_i8_oversample, parse_measured_iterations, parse_metric,
    parse_query_f16_exact, parse_rows, parse_shader_ab, parse_top_k, readback_bytes_per_call,
    rescored_candidates_per_call, round_to_f16_exact, speedup_ratio, BenchMetric,
    GpuScalingI8Result, GpuScalingI8StatsLine, GpuScalingRecallLine, GpuScalingResult,
    GpuScalingStatsLine,
};
use std::time::Duration;

// ---------------------------------------------------------------------
// parse_rows / parse_dims / parse_batches
// ---------------------------------------------------------------------

#[test]
fn parse_rows_defaults_when_unset_or_empty() {
    let default = [20_000, 100_000, 500_000];
    assert_eq!(parse_rows(None, &default), Ok(default.to_vec()));
    assert_eq!(parse_rows(Some(""), &default), Ok(default.to_vec()));
    assert_eq!(parse_rows(Some("   "), &default), Ok(default.to_vec()));
}

#[test]
fn parse_rows_accepts_comma_separated_list_with_whitespace() {
    let default = [1];
    assert_eq!(
        parse_rows(Some(" 10, 20 ,30"), &default),
        Ok(vec![10, 20, 30])
    );
}

#[test]
fn parse_rows_rejects_zero_non_numeric_and_out_of_bound_fail_closed() {
    let default = [1];
    for raw in ["0", "abc", "1.5", "-1", ""] {
        if raw.is_empty() {
            continue; // 空文字列は既定値として受理される契約（別テストで検証済み）
        }
        assert!(
            parse_rows(Some(raw), &default).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
    // 上限（5,000,000）超過は拒否する。
    assert!(parse_rows(Some("5000001"), &default).is_err());
}

#[test]
fn parse_rows_rejects_too_many_entries_fail_closed() {
    let default = [1];
    let too_many = (0..17)
        .map(|i| (i + 1).to_string())
        .collect::<Vec<_>>()
        .join(",");
    assert!(parse_rows(Some(&too_many), &default).is_err());
}

#[test]
fn parse_dims_and_parse_batches_share_the_same_list_parser() {
    assert_eq!(parse_dims(Some("128,256"), &[1]), Ok(vec![128, 256]));
    assert_eq!(parse_batches(Some("1,8,64"), &[1]), Ok(vec![1, 8, 64]));
    assert!(parse_dims(Some("0"), &[1]).is_err());
    assert!(parse_batches(Some("-5"), &[1]).is_err());
}

// ---------------------------------------------------------------------
// parse_top_k / parse_measured_iterations
// ---------------------------------------------------------------------

#[test]
fn parse_top_k_defaults_and_accepts_within_bound() {
    assert_eq!(parse_top_k(None, 10), Ok(10));
    assert_eq!(parse_top_k(Some(""), 10), Ok(10));
    assert_eq!(parse_top_k(Some("5"), 10), Ok(5));
}

#[test]
fn parse_top_k_rejects_zero_non_numeric_and_over_bound_fail_closed() {
    assert!(parse_top_k(Some("0"), 10).is_err());
    assert!(parse_top_k(Some("abc"), 10).is_err());
    assert!(parse_top_k(Some("10001"), 10).is_err());
}

#[test]
fn parse_measured_iterations_defaults_and_accepts_within_bound() {
    assert_eq!(parse_measured_iterations(None, 20), Ok(20));
    assert_eq!(parse_measured_iterations(Some(""), 20), Ok(20));
    assert_eq!(parse_measured_iterations(Some("50"), 20), Ok(50));
}

#[test]
fn parse_measured_iterations_rejects_below_protocol_minimum_and_non_numeric() {
    assert!(parse_measured_iterations(Some("19"), 20).is_err());
    assert!(parse_measured_iterations(Some("abc"), 20).is_err());
    assert!(parse_measured_iterations(Some("-1"), 20).is_err());
}

// ---------------------------------------------------------------------
// speedup_ratio
// ---------------------------------------------------------------------

#[test]
fn speedup_ratio_computes_cpu_over_gpu() {
    let cpu = Duration::from_micros(1000);
    let gpu = Duration::from_micros(250);
    let ratio = speedup_ratio(cpu, gpu).expect("non-zero denominator");
    assert!((ratio - 4.0).abs() < 1e-9);
}

#[test]
fn speedup_ratio_rejects_zero_denominator_fail_closed() {
    assert!(speedup_ratio(Duration::from_micros(1000), Duration::ZERO).is_err());
}

// ---------------------------------------------------------------------
// count_boundary_tolerant_mismatches
// ---------------------------------------------------------------------

#[test]
fn mismatches_are_zero_when_candidate_matches_baseline_ids_exactly() {
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    let candidate = vec![(3u64, 1.0f32), (1, 3.0), (2, 2.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 0);
}

#[test]
fn mismatches_tolerate_ties_at_the_boundary_score() {
    // baseline の境界スコア（最小値）は 1.0。candidate の id=9 は baseline に
    // 含まれないが、スコアが境界と同値（同点）のため不一致に数えない。
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    let candidate = vec![(1u64, 3.0f32), (2, 2.0), (9, 1.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 0);
}

#[test]
fn mismatches_count_ids_below_boundary_and_absent_from_baseline() {
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    // id=9 は baseline に無くスコアも境界(1.0)未満のため不一致。
    let candidate = vec![(1u64, 3.0f32), (2, 2.0), (9, 0.5)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
}

#[test]
fn mismatches_with_empty_baseline_counts_all_candidates_fail_closed() {
    let baseline: Vec<(u64, f32)> = Vec::new();
    let candidate = vec![(1u64, 1.0f32), (2, 2.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 2);
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &[]), 0);
}

#[test]
fn mismatches_count_missing_hits_when_candidate_is_shorter_than_baseline() {
    // GPU 経路が Top-k を取りこぼした（候補が短い・空）場合も欠落分を不一致に
    // 数える。余分な候補だけを数えると取りこぼしが `mismatch=0` に見えてしまう。
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    let candidate = vec![(1u64, 3.0f32), (2, 2.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &[]), 3);
    // 欠落と境界未満の余分な候補は加算される。
    let candidate = vec![(1u64, 3.0f32), (9, 0.5)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 2);
}

#[test]
fn mismatches_count_baseline_ids_above_boundary_missing_from_candidate() {
    // 境界スコア（1.0）と同点の id=9 が、境界より上位の id=2（2.0）を置き換えている。
    // 件数は一致し id=9 は同点許容の対象だが、上位 id の欠落は不一致に数える。
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    let candidate = vec![(1u64, 3.0f32), (3, 1.0), (9, 1.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
    // 境界上（1.0）の id=3 が同点の id=9 に置き換わるだけなら許容する。
    let candidate = vec![(1u64, 3.0f32), (2, 2.0), (9, 1.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 0);
}

#[test]
fn mismatches_do_not_double_count_extras_or_duplicates_within_excess() {
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    // 境界未満の余分な 1 件（`extra`）で候補が長くなった場合、超過分として
    // 二重に数えない（Bugbot 指摘）。
    let candidate = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0), (9, 0.5)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
    // 重複で候補が長くなった場合も同様に 1 件のみ。
    let candidate = vec![(1u64, 3.0f32), (1, 3.0), (2, 2.0), (3, 1.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
    // 境界同点の水増し 1 件＋境界未満の余分 1 件は合計 2 件。
    let candidate = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0), (4, 1.0), (9, 0.5)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 2);
}

#[test]
fn mismatches_count_duplicate_ids_in_candidate() {
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    // 長さは一致するが id=1 が重複しており id=3 が欠けている → 重複 1 件。
    let candidate = vec![(1u64, 3.0f32), (1, 3.0), (2, 2.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
}

#[test]
fn mismatches_count_excess_candidates_beyond_baseline_length() {
    // codex-review P2: baseline の全件を含み境界同点の id=4 を 1 件追加しただけの
    // candidate（境界スコア未満ではないため `extra` に捕捉されない）が、件数超過
    // チェック無しでは不一致 0 になっていた。件数超過は不一致として計上する。
    let baseline = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0)];
    let candidate = vec![(1u64, 3.0f32), (2, 2.0), (3, 1.0), (4, 1.0)];
    assert_eq!(count_boundary_tolerant_mismatches(&baseline, &candidate), 1);
}

// ---------------------------------------------------------------------
// 出力整形
// ---------------------------------------------------------------------

#[test]
fn format_skip_line_includes_all_dimensions_and_reason() {
    let line = format_skip_line(500_000, 256, 256, 10, "exceeds MAX_BATCH_WORK");
    assert!(line.starts_with("gpu_scaling: skip "));
    assert!(line.contains("rows=500000"));
    assert!(line.contains("dim=256"));
    assert!(line.contains("batch=256"));
    assert!(line.contains("k=10"));
    assert!(line.contains("exceeds MAX_BATCH_WORK"));
}

#[test]
fn format_unavailable_line_distinguishes_process_wide_and_combo_scoped_failures() {
    let process_wide = format_unavailable_line(None, "adapter request timed out");
    assert_eq!(
        process_wide,
        "gpu_scaling: gpu unavailable (adapter request timed out)"
    );

    let combo_scoped = format_unavailable_line(Some((500_000, 256, 256, 10)), "buffer too large");
    assert!(combo_scoped.starts_with("gpu_scaling: not measurable "));
    assert!(combo_scoped.contains("rows=500000"));
    assert!(combo_scoped.contains("buffer too large"));
}

#[test]
fn result_line_contains_all_required_fields() {
    let result = GpuScalingResult {
        rows: 100_000,
        dim: 256,
        batch: 64,
        k: 10,
        cpu_simd_p50: Duration::from_micros(500),
        cpu_simd_p95: Duration::from_micros(700),
        gpu_f16_p50: Duration::from_micros(100),
        gpu_f16_p95: Duration::from_micros(150),
        gpu_f32_p50: Duration::from_micros(120),
        gpu_f32_p95: Duration::from_micros(180),
        per_query_cpu_p50: Duration::from_micros(500) / 64,
        per_query_gpu_f16_p50: Duration::from_micros(100) / 64,
        speedup_f16_p95: speedup_ratio(Duration::from_micros(700), Duration::from_micros(150))
            .expect("non-zero denominator"),
        mismatch: 0,
    };
    let line = result.to_string();
    for expected in [
        "gpu_scaling:",
        "rows=100000",
        "dim=256",
        "batch=64",
        "k=10",
        "cpu_simd_p50=",
        "cpu_simd_p95=",
        "gpu_f16_p50=",
        "gpu_f16_p95=",
        "gpu_f32_p50=",
        "gpu_f32_p95=",
        "per_query_cpu_p50=",
        "per_query_gpu_f16_p50=",
        "speedup_f16_p95=",
        "mismatch=0",
    ] {
        assert!(line.contains(expected), "missing {expected:?} in {line:?}");
    }
}

// ---------------------------------------------------------------------
// 読み戻しバイト数の前後比較（Issue #537）向け純関数
// ---------------------------------------------------------------------

#[test]
fn readback_bytes_per_call_divides_total_by_calls() {
    assert_eq!(readback_bytes_per_call(1_000, 10).expect("ok"), 100);
    // 割り切れない場合は切り捨て（整数除算。呼び出し元は「1 回あたり」の
    // 概算値として扱う契約）。
    assert_eq!(readback_bytes_per_call(1_001, 10).expect("ok"), 100);
}

#[test]
fn readback_bytes_per_call_rejects_zero_calls() {
    assert!(readback_bytes_per_call(1_000, 0).is_err());
}

#[test]
fn full_readback_bytes_estimate_matches_rows_times_batch_times_four() {
    assert_eq!(
        full_readback_bytes_estimate(100_000, 64).expect("ok"),
        100_000u64 * 64 * 4
    );
    assert_eq!(full_readback_bytes_estimate(0, 64).expect("ok"), 0);
    assert_eq!(full_readback_bytes_estimate(100, 0).expect("ok"), 0);
}

#[test]
fn full_readback_bytes_estimate_rejects_overflow() {
    // rows・batch のいずれも usize::MAX 級であれば `* 4` で確実にオーバー
    // フローする組み合わせを与え、fail-closed に拒否されることを固定する
    // （無音のラップアラウンドで小さい「算出値」を返さない）。
    assert!(full_readback_bytes_estimate(usize::MAX, 2).is_err());
}

#[test]
fn gpu_scaling_stats_line_display_contains_all_required_fields() {
    let line = GpuScalingStatsLine {
        rows: 100_000,
        dim: 128,
        batch: 64,
        k: 10,
        calls: 40,
        f16_readback_bytes_total: 1_024,
        f16_readback_bytes_per_call: 25,
        f16_partial_topk_dispatches: 40,
        f16_full_readback_dispatches: 0,
        f16_full_readback_fallbacks: 0,
        f16_arith_dispatches: 40,
        f16_arith_guard_fallbacks: 0,
        f32_readback_bytes_total: 2_048,
        f32_readback_bytes_per_call: 51,
        f32_partial_topk_dispatches: 40,
        f32_full_readback_dispatches: 0,
        f32_full_readback_fallbacks: 0,
    };
    let rendered = line.to_string();
    for expected in [
        "gpu_scaling_stats:",
        "rows=100000",
        "dim=128",
        "batch=64",
        "k=10",
        "calls=40",
        "f16_readback_bytes_total=1024",
        "f16_readback_bytes_per_call=25",
        "f16_partial_topk_dispatches=40",
        "f16_full_readback_dispatches=0",
        "f16_full_readback_fallbacks=0",
        "f16_arith_dispatches=40",
        "f16_arith_guard_fallbacks=0",
        "f32_readback_bytes_total=2048",
        "f32_readback_bytes_per_call=51",
        "f32_partial_topk_dispatches=40",
        "f32_full_readback_dispatches=0",
        "f32_full_readback_fallbacks=0",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected:?} in {rendered:?}"
        );
    }
}

#[test]
fn gpu_scaling_stats_line_prefix_does_not_collide_with_result_line_grep() {
    // `scripts/bench_gpu_scaling_ab.sh` は結果行を `^gpu_scaling: rows=` で
    // grep する（PR #580 由来）。統計行の接頭辞 `gpu_scaling_stats: ` は
    // この前方一致パターンにマッチしないことを固定し、既存 A/B 集計
    // スクリプトへ統計行が結果行として誤って取り込まれないことを保証する。
    let line = GpuScalingStatsLine {
        rows: 1,
        dim: 1,
        batch: 1,
        k: 1,
        calls: 1,
        f16_readback_bytes_total: 0,
        f16_readback_bytes_per_call: 0,
        f16_partial_topk_dispatches: 0,
        f16_full_readback_dispatches: 0,
        f16_full_readback_fallbacks: 0,
        f16_arith_dispatches: 0,
        f16_arith_guard_fallbacks: 0,
        f32_readback_bytes_total: 0,
        f32_readback_bytes_per_call: 0,
        f32_partial_topk_dispatches: 0,
        f32_full_readback_dispatches: 0,
        f32_full_readback_fallbacks: 0,
    };
    let rendered = line.to_string();
    assert!(!rendered.starts_with("gpu_scaling: rows="));
}

// ---------------------------------------------------------------------
// i8 経路（Issue #543）: parse_i8_oversample / GpuScalingI8Result /
// GpuScalingI8StatsLine / mean_recall_at_k / rescored_candidates_per_call
// ---------------------------------------------------------------------

#[test]
fn parse_i8_oversample_defaults_when_unset_or_empty() {
    assert_eq!(parse_i8_oversample(None, 4, 32), Ok(4));
    assert_eq!(parse_i8_oversample(Some(""), 4, 32), Ok(4));
    assert_eq!(parse_i8_oversample(Some("  "), 4, 32), Ok(4));
}

#[test]
fn parse_i8_oversample_accepts_boundary_values() {
    assert_eq!(parse_i8_oversample(Some("1"), 4, 32), Ok(1));
    assert_eq!(parse_i8_oversample(Some("32"), 4, 32), Ok(32));
}

#[test]
fn parse_i8_oversample_rejects_zero_out_of_range_and_non_numeric_fail_closed() {
    for raw in ["0", "33", "abc", "-1", "4.5"] {
        assert!(
            parse_i8_oversample(Some(raw), 4, 32).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
}

#[test]
fn mean_recall_at_k_rejects_empty_and_averages_correctly() {
    assert!(mean_recall_at_k(&[]).is_err());
    let mean = mean_recall_at_k(&[1.0, 0.5, 0.75]).expect("non-empty input should succeed");
    assert!((mean - 0.75).abs() < 1e-12);
}

#[test]
fn rescored_candidates_per_call_rejects_zero_calls_fail_closed() {
    assert!(rescored_candidates_per_call(100, 0).is_err());
    assert_eq!(rescored_candidates_per_call(100, 10), Ok(10));
}

#[test]
fn gpu_scaling_i8_result_line_has_expected_prefix_and_fields() {
    let result = GpuScalingI8Result {
        rows: 20_000,
        dim: 128,
        batch: 8,
        k: 10,
        oversample: 4,
        gpu_i8_p50: Duration::from_micros(100),
        gpu_i8_p95: Duration::from_micros(150),
        per_query_gpu_i8_p50: Duration::from_micros(12),
        speedup_i8_vs_cpu_p95: 2.5,
        speedup_i8_vs_f16_p95: 1.1,
        i8_mismatch: 0,
        i8_recall_at_k: 0.9750,
    };
    let rendered = result.to_string();
    assert!(rendered.starts_with("gpu_scaling_i8: rows=20000 dim=128 batch=8 k=10 oversample=4"));
    assert!(rendered.contains("i8_recall_at_k=0.9750"));
    assert!(rendered.contains("i8_mismatch=0"));
    // 既存の結果行 grep（`^gpu_scaling: rows=`）に誤マッチしないこと。
    assert!(!rendered.starts_with("gpu_scaling: rows="));
}

#[test]
fn format_i8_unavailable_line_has_expected_prefix() {
    let line = format_i8_unavailable_line(20_000, 128, 8, 10, 4, "gpu i8 backend init failed");
    assert!(line.starts_with(
        "gpu_scaling_i8: not measurable rows=20000 dim=128 batch=8 k=10 oversample=4"
    ));
    assert!(line.contains("gpu i8 backend init failed"));
}

// ---------------------------------------------------------------------
// Issue #540: f16 厳密往復クエリ opt-in（`BENCH_GPU_SCALING_QUERY_F16_EXACT`）
// ---------------------------------------------------------------------

#[test]
fn parse_query_f16_exact_defaults_to_false() {
    assert_eq!(parse_query_f16_exact(None), Ok(false));
    assert_eq!(parse_query_f16_exact(Some("")), Ok(false));
}

#[test]
fn parse_query_f16_exact_accepts_only_literal_one() {
    assert_eq!(parse_query_f16_exact(Some("1")), Ok(true));
}

#[test]
fn parse_query_f16_exact_rejects_other_values_fail_closed() {
    assert!(parse_query_f16_exact(Some("true")).is_err());
    assert!(parse_query_f16_exact(Some("0")).is_err());
    assert!(parse_query_f16_exact(Some("yes")).is_err());
}

// ---------------------------------------------------------------------
// Issue #540 追記（codex-review P2 指摘対応・PR #611）: 同一クエリでの
// シェーダ単体比較 opt-in（`BENCH_GPU_SCALING_SHADER_AB`）
// ---------------------------------------------------------------------

#[test]
fn parse_shader_ab_defaults_to_false() {
    assert_eq!(parse_shader_ab(None), Ok(false));
    assert_eq!(parse_shader_ab(Some("")), Ok(false));
}

#[test]
fn parse_shader_ab_accepts_only_literal_one() {
    assert_eq!(parse_shader_ab(Some("1")), Ok(true));
}

#[test]
fn parse_shader_ab_rejects_other_values_fail_closed() {
    assert!(parse_shader_ab(Some("true")).is_err());
    assert!(parse_shader_ab(Some("0")).is_err());
    assert!(parse_shader_ab(Some("yes")).is_err());
}

#[test]
fn round_to_f16_exact_is_idempotent() {
    // 一度丸めた値は再度丸めても不変であること（f16 表現できる値の固定点）。
    let samples = [0.0f32, 1.0, -1.0, 0.5, 123.25, -7.0, 1e-3, 1e4];
    for &v in &samples {
        let once = round_to_f16_exact(v);
        let twice = round_to_f16_exact(once);
        assert_eq!(once, twice, "not a fixed point for input {v}");
    }
}

#[test]
fn round_to_f16_exact_zero_stays_zero() {
    assert_eq!(round_to_f16_exact(0.0), 0.0);
    assert_eq!(round_to_f16_exact(-0.0), -0.0);
}

#[test]
fn gpu_scaling_i8_stats_line_prefix_does_not_collide_with_other_line_greps() {
    let line = GpuScalingI8StatsLine {
        rows: 1,
        dim: 1,
        batch: 1,
        k: 1,
        oversample: 4,
        calls: 1,
        readback_bytes_total: 0,
        readback_bytes_per_call: 0,
        rescored_candidates_total: 0,
        rescored_candidates_per_call: 0,
        backend: "Vulkan".to_string(),
        dot4_impl: "Undetermined".to_string(),
        build_ms: 5,
    };
    let rendered = line.to_string();
    assert!(rendered.starts_with("gpu_scaling_i8_stats: rows="));
    assert!(!rendered.starts_with("gpu_scaling: rows="));
    assert!(!rendered.starts_with("gpu_scaling_i8: rows="));
}
// ---------------------------------------------------------------------
// parse_metric / format_metric_line / GpuScalingRecallLine（Issue #2005）
// ---------------------------------------------------------------------

#[test]
fn parse_metric_defaults_to_inner_product_when_unset_or_blank() {
    assert_eq!(parse_metric(None), Ok(BenchMetric::InnerProduct));
    assert_eq!(parse_metric(Some("")), Ok(BenchMetric::InnerProduct));
    assert_eq!(parse_metric(Some("  ")), Ok(BenchMetric::InnerProduct));
}

#[test]
fn parse_metric_accepts_the_three_allowed_tokens_with_trim() {
    assert_eq!(
        parse_metric(Some("inner_product")),
        Ok(BenchMetric::InnerProduct)
    );
    assert_eq!(parse_metric(Some("l2")), Ok(BenchMetric::L2));
    assert_eq!(parse_metric(Some(" cosine ")), Ok(BenchMetric::Cosine));
}

#[test]
fn parse_metric_rejects_l1_and_unknown_values_fail_closed() {
    for raw in ["l1", "L2", "Cosine", "foo", "ip", "l2;cosine"] {
        assert!(parse_metric(Some(raw)).is_err(), "must reject {raw:?}");
    }
    let msg = parse_metric(Some("l1")).unwrap_err().to_string();
    assert!(msg.contains("l1"));
}

#[test]
fn bench_metric_tokens_round_trip_through_parse_metric() {
    for m in [
        BenchMetric::InnerProduct,
        BenchMetric::L2,
        BenchMetric::Cosine,
    ] {
        assert_eq!(parse_metric(Some(m.as_str())), Ok(m));
    }
}

#[test]
fn metric_line_and_recall_line_use_distinct_prefixes() {
    assert_eq!(
        format_metric_line(BenchMetric::L2),
        "gpu_scaling_metric: metric=l2"
    );
    let rendered = GpuScalingRecallLine {
        rows: 20_000,
        dim: 128,
        batch: 8,
        k: 10,
        metric: BenchMetric::Cosine,
        f16_recall: 0.987_654_321,
        f32_recall: 1.0,
    }
    .to_string();
    assert_eq!(
        rendered,
        "gpu_scaling_recall: rows=20000 dim=128 batch=8 k=10 metric=cosine \
         f16_recall=0.987654 f32_recall=1.000000"
    );
    // 既存行の grep（`gpu_scaling: rows=` 等）と接頭辞が衝突しない。
    assert!(!rendered.starts_with("gpu_scaling: rows="));
    assert!(!format_metric_line(BenchMetric::L2).starts_with("gpu_scaling: rows="));
}
