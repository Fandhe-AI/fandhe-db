//! `benches/harness/chip.rs`（Issue #469。チップ別手動計測オーケストレータ
//! `chip_bench.rs` が使う純関数群）の回帰テスト。
//!
//! `chip_bench_bin` 自体は時間依存（子プロセス実行）のためこのテストからは
//! 実行しない。`#[path]` で取り込んだ純関数（env パース・行パーサ・最小 JSON
//! パーサ・集計・環境情報パーサ・JSON 生成）のみを `cargo test`（`make ci` 対象）
//! で時間非依存に検証する（`dot_kernel_accept.rs`・`bench_engine_accept.rs` と
//! 同型の構成）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::chip::{
    aggregate, dedicated_env_attested, json_escape, json_number, parse_cache_size,
    parse_dot_kernel_diag_line, parse_dot_kernel_line, parse_feature_bench_output, parse_json,
    parse_knn_stage_line, parse_proc_cpuinfo, parse_rounds, parse_sysctl_lines, parse_workloads,
    refuse_under_github_actions, ChipError, JsonValue, Workload, AARCH64_INTEREST_FLAGS,
    DEFAULT_ROUNDS, JSON_MAX_INPUT_BYTES, MAX_ROUNDS, X86_INTEREST_FLAGS,
};

// --- refuse_under_github_actions ---

#[test]
fn refuse_under_github_actions_rejects_when_true() {
    assert_eq!(
        refuse_under_github_actions(true).unwrap_err(),
        ChipError::RefusedUnderGitHubActions
    );
}

#[test]
fn refuse_under_github_actions_allows_when_false() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- parse_rounds ---

#[test]
fn parse_rounds_defaults_when_unset_or_empty() {
    assert_eq!(parse_rounds(None), Ok(DEFAULT_ROUNDS));
    assert_eq!(parse_rounds(Some("")), Ok(DEFAULT_ROUNDS));
}

#[test]
fn parse_rounds_trims_whitespace_and_accepts_within_bound() {
    assert_eq!(parse_rounds(Some(" 3 ")), Ok(3));
    assert_eq!(parse_rounds(Some("1")), Ok(1));
    assert_eq!(parse_rounds(Some("50")), Ok(MAX_ROUNDS));
}

#[test]
fn parse_rounds_rejects_zero_over_bound_and_non_numeric() {
    for raw in ["0", "51", "abc", "1.5", "-1"] {
        assert!(
            parse_rounds(Some(raw)).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
}

// --- parse_workloads ---

#[test]
fn parse_workloads_defaults_to_all_four_in_fixed_order() {
    let all = parse_workloads(None).unwrap();
    assert_eq!(
        all,
        vec![
            Workload::DotKernel,
            Workload::KnnProfile,
            Workload::Feature128,
            Workload::Feature768,
        ]
    );
    assert_eq!(parse_workloads(Some("")).unwrap(), all);
}

#[test]
fn parse_workloads_accepts_subset_with_whitespace() {
    let subset = parse_workloads(Some(" dot_kernel , feature_768 ")).unwrap();
    assert_eq!(subset, vec![Workload::DotKernel, Workload::Feature768]);
}

#[test]
fn parse_workloads_rejects_unknown_duplicate_and_empty_element() {
    assert!(parse_workloads(Some("foo")).is_err());
    assert!(parse_workloads(Some("dot_kernel,dot_kernel")).is_err());
    assert!(parse_workloads(Some("dot_kernel,,knn_profile")).is_err());
}

#[test]
fn workload_extra_env_sets_dim_only_for_feature_variants() {
    assert!(Workload::DotKernel.extra_env().is_empty());
    assert!(Workload::KnnProfile.extra_env().is_empty());
    assert_eq!(
        Workload::Feature128.extra_env(),
        &[("BENCH_FEATURE_DIM", "128")]
    );
    assert_eq!(
        Workload::Feature768.extra_env(),
        &[("BENCH_FEATURE_DIM", "768")]
    );
}

// --- dedicated_env_attested ---

#[test]
fn dedicated_env_attested_requires_exact_one() {
    assert!(dedicated_env_attested(Some("1")));
    assert!(dedicated_env_attested(Some(" 1 ")));
    assert!(!dedicated_env_attested(Some("true")));
    assert!(!dedicated_env_attested(None));
    assert!(!dedicated_env_attested(Some("0")));
}

// --- parse_dot_kernel_line / parse_dot_kernel_diag_line ---

#[test]
fn parse_dot_kernel_line_reads_current_label_line() {
    let line =
        "dot_kernel: label=current working_set=cache_resident dim=768 rows=200 median_ms=0.189 ns_per_dot=44.88";
    let sample = parse_dot_kernel_line(line).unwrap();
    assert_eq!(sample.working_set, "cache_resident");
    assert_eq!(sample.dim, 768);
    assert_eq!(sample.rows, 200);
    assert_eq!(sample.median_ms, 0.189);
    assert_eq!(sample.ns_per_dot, 44.88);
}

#[test]
fn parse_dot_kernel_line_ignores_other_lines() {
    assert!(parse_dot_kernel_line("env: os=linux arch=x86_64").is_none());
    assert!(parse_dot_kernel_line(
        "dot_kernel: diagnostic_ab dim=768 rows=200 simd_vs_scalar_ratio=0.179 class=Improved"
    )
    .is_none());
}

#[test]
fn parse_dot_kernel_diag_line_reads_diagnostic_ab_line() {
    let line =
        "dot_kernel: diagnostic_ab dim=768 rows=200 simd_vs_scalar_ratio=0.179 class=Improved";
    let diag = parse_dot_kernel_diag_line(line).unwrap();
    assert_eq!(diag.dim, 768);
    assert_eq!(diag.rows, 200);
    assert_eq!(diag.simd_vs_scalar_ratio, 0.179);
    assert_eq!(diag.class, "Improved");
}

// --- parse_knn_stage_line ---

#[test]
fn parse_knn_stage_line_reads_stage_line_with_ms_suffix() {
    let line = "stage(S1_redb_scan): rows=25000 median=0.976ms ns_per_row=39.1";
    let sample = parse_knn_stage_line(line).unwrap();
    assert_eq!(sample.name, "S1_redb_scan");
    assert_eq!(sample.rows, 25000);
    assert_eq!(sample.median_ms, 0.976);
    assert_eq!(sample.ns_per_row, 39.1);
}

#[test]
fn parse_knn_stage_line_ignores_diff_and_residual_lines() {
    assert!(parse_knn_stage_line("diff(S1->S2): ns_per_row=5.5").is_none());
    assert!(parse_knn_stage_line(
        "residual(S0-(S4+S5)): median=1.234ms (parse/bind/result-assembly 等)"
    )
    .is_none());
}

// --- 最小 JSON パーサ ---

#[test]
fn parse_json_reads_object_array_string_number_bool_null() {
    let text = r#"{"a": 1, "b": [1, 2.5, -3], "c": "hi\n\"there\"", "d": true, "e": null}"#;
    let value = parse_json(text).unwrap();
    assert_eq!(value.get("a").unwrap().as_f64(), Some(1.0));
    let arr = value.get("b").unwrap().as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[2].as_f64(), Some(-3.0));
    assert_eq!(value.get("c").unwrap().as_str(), Some("hi\n\"there\""));
    assert_eq!(value.get("d").unwrap(), &JsonValue::Bool(true));
    assert_eq!(value.get("e").unwrap(), &JsonValue::Null);
}

#[test]
fn parse_json_handles_nested_objects() {
    let text = r#"{"meta":{"dim":128,"nested":{"x":1}},"phases":[{"name":"p1"}]}"#;
    let value = parse_json(text).unwrap();
    let meta = value.get("meta").unwrap();
    assert_eq!(meta.get("dim").unwrap().as_f64(), Some(128.0));
    assert_eq!(
        meta.get("nested").unwrap().get("x").unwrap().as_f64(),
        Some(1.0)
    );
}

#[test]
fn parse_json_preserves_non_ascii_utf8_in_unescaped_strings() {
    // 非エスケープ区間はバイト単位で `char` へ変換せず UTF-8 文字列スライスの
    // まま保持する契約（codex-review 指摘。crates/engine/benches/harness/chip.rs）。
    let text = r#"{"label":"日本語","mixed":"a日b語c"}"#;
    let value = parse_json(text).unwrap();
    assert_eq!(value.get("label").unwrap().as_str(), Some("日本語"));
    assert_eq!(value.get("mixed").unwrap().as_str(), Some("a日b語c"));
}

#[test]
fn parse_json_rejects_invalid_input() {
    assert!(parse_json("{").is_err());
    assert!(parse_json("").is_err());
    assert!(parse_json("{\"a\":1} trailing").is_err());
    assert!(parse_json("not json").is_err());
}

#[test]
fn parse_json_rejects_input_exceeding_max_bytes() {
    let huge = "a".repeat(JSON_MAX_INPUT_BYTES + 1);
    let text = format!("\"{huge}\"");
    assert!(parse_json(&text).is_err());
}

#[test]
fn parse_json_rejects_recursion_beyond_max_depth() {
    // 65 段のネスト配列（上限 64 を超過）。
    let mut text = String::new();
    for _ in 0..65 {
        text.push('[');
    }
    text.push('1');
    for _ in 0..65 {
        text.push(']');
    }
    assert!(parse_json(&text).is_err());
}

// --- parse_feature_bench_output ---

const FEATURE_BENCH_FIXTURE: &str = r#"{"meta":{"rows_tenant_a":12500,"rows_tenant_b":12500,"dim":128,"batch_size":500,"warmup":2,"iters":10,"db_bytes_after_ingest":100,"db_bytes_final":200,"vm_rss_kb_final":1000,"vm_hwm_kb_final":2000,"label":"feature_bench","engine":"brute_force","scale":1,"rows_total":25000,"index_warm_us":0},"phases":[{"name":"ingest","iterations":10,"min_us":3949,"p50_us":4064,"p95_us":4194,"p99_us":4200,"max_us":4300,"mean_us":4070.5,"rss_kb_after":1000,"cpu_tick_delta":5,"extra":""},{"name":"vector_knn","iterations":10,"min_us":8359,"p50_us":8622,"p95_us":9549,"p99_us":9600,"max_us":9700,"mean_us":8700.0,"rss_kb_after":1000,"cpu_tick_delta":5,"extra":""}]}"#;

#[test]
fn parse_feature_bench_output_reads_last_meta_line() {
    let stdout = format!("some noise line\n{FEATURE_BENCH_FIXTURE}\n");
    let result = parse_feature_bench_output(&stdout, 128).unwrap();
    assert_eq!(result.dim, 128);
    assert_eq!(result.engine, "brute_force");
    assert_eq!(result.scale, 1);
    assert_eq!(result.rows_total, 25000);
    assert_eq!(result.phases.len(), 2);
    assert_eq!(result.phases[0].name, "ingest");
    assert_eq!(result.phases[0].min_us, 3949.0);
    assert_eq!(result.phases[1].p95_us, 9549.0);
}

#[test]
fn parse_feature_bench_output_rejects_dim_mismatch() {
    let stdout = FEATURE_BENCH_FIXTURE.to_string();
    assert!(parse_feature_bench_output(&stdout, 768).is_err());
}

#[test]
fn parse_feature_bench_output_rejects_missing_meta_line() {
    assert!(parse_feature_bench_output("no meta line here\n", 128).is_err());
}

// --- aggregate ---

#[test]
fn aggregate_computes_min_median_max_and_reference_band() {
    let series = aggregate(&[10.0, 20.0, 30.0]).unwrap();
    assert_eq!(series.min, 10.0);
    assert_eq!(series.median, 20.0);
    assert_eq!(series.max, 30.0);
    assert_eq!(series.reference_band_pct, Some(200.0));
}

#[test]
fn aggregate_computes_median_for_even_length() {
    let series = aggregate(&[10.0, 20.0, 30.0, 40.0]).unwrap();
    assert_eq!(series.median, 25.0);
}

#[test]
fn aggregate_reference_band_pct_is_none_when_min_is_zero() {
    // codex-review 指摘（PR #560）: min が 0 の系列は分母ゼロで算出不能。
    // ばらつきが実際に 0 な `Some(0.0)` と区別できるよう `None` を返す。
    let series = aggregate(&[0.0, 1.0]).unwrap();
    assert_eq!(series.min, 0.0);
    assert_eq!(series.reference_band_pct, None);
}

#[test]
fn aggregate_reference_band_pct_is_some_zero_when_no_variance() {
    // min == max（かつ非ゼロ）はばらつきが実際に 0 のケースであり、
    // 算出不能（None）とは区別して `Some(0.0)` を返す。
    let series = aggregate(&[5.0, 5.0, 5.0]).unwrap();
    assert_eq!(series.reference_band_pct, Some(0.0));
}

#[test]
fn aggregate_rejects_empty_series() {
    assert!(aggregate(&[]).is_err());
}

// --- cpuinfo / sysctl / cache size パーサ ---

#[test]
fn parse_proc_cpuinfo_reads_model_name_and_filters_flags() {
    let text = "processor\t: 0\nmodel name\t: QEMU Virtual CPU version 2.5+\nflags\t\
        : fpu vme de pse sse4_2 avx avx2 fma f16c unrelated_flag\n";
    let info = parse_proc_cpuinfo(text, X86_INTEREST_FLAGS);
    assert_eq!(
        info.model_name.as_deref(),
        Some("QEMU Virtual CPU version 2.5+")
    );
    assert_eq!(info.flags, vec!["sse4_2", "avx", "avx2", "fma", "f16c"]);
    assert!(!info.flags.iter().any(|f| f == "unrelated_flag"));
}

#[test]
fn parse_proc_cpuinfo_aarch64_reads_features_line() {
    let text = "Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 asimddp\n";
    let info = parse_proc_cpuinfo(text, AARCH64_INTEREST_FLAGS);
    assert_eq!(info.flags, vec!["asimd", "asimddp"]);
}

#[test]
fn parse_sysctl_lines_reads_key_value_pairs() {
    let text = "machdep.cpu.brand_string: Apple M2\nhw.ncpu: 8\n";
    let pairs = parse_sysctl_lines(text);
    assert_eq!(
        pairs,
        vec![
            (
                "machdep.cpu.brand_string".to_string(),
                "Apple M2".to_string()
            ),
            ("hw.ncpu".to_string(), "8".to_string()),
        ]
    );
}

#[test]
fn parse_cache_size_reads_k_m_g_suffixes_and_plain_numbers() {
    assert_eq!(parse_cache_size("32K"), Some(32 * 1024));
    assert_eq!(parse_cache_size("48M"), Some(48 * 1024 * 1024));
    assert_eq!(parse_cache_size("1G"), Some(1024 * 1024 * 1024));
    assert_eq!(parse_cache_size("1024"), Some(1024));
    assert_eq!(parse_cache_size("not a size"), None);
}

// --- JSON 生成 ---

#[test]
fn json_escape_escapes_quotes_backslashes_and_control_chars() {
    assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
    assert_eq!(json_escape("line1\nline2"), "line1\\nline2");
}

#[test]
fn json_number_formats_finite_and_maps_non_finite_to_null() {
    assert_eq!(json_number(1.5), "1.500000");
    assert_eq!(json_number(f64::NAN), "null");
    assert_eq!(json_number(f64::INFINITY), "null");
}

#[test]
fn json_escape_output_round_trips_through_parse_json() {
    let original = "quote\"back\\slash\nnewline";
    let escaped = json_escape(original);
    let wrapped = format!("\"{escaped}\"");
    let value = parse_json(&wrapped).unwrap();
    assert_eq!(value.as_str(), Some(original));
}

// --- Workload::cargo_args（Issue #1482: 子プロセスのパッケージ名） ---

/// 子プロセス argv の `-p` が現行パッケージ名（`CARGO_PKG_NAME`）を指し、
/// 改名前の `engine` 単独要素を含まず、bench/example 名が維持されること。
#[test]
fn workload_cargo_args_use_current_package_name() {
    let pkg = env!("CARGO_PKG_NAME");
    assert_eq!(pkg, "fandhe-db-engine");
    for w in parse_workloads(None).unwrap() {
        let args = w.cargo_args(pkg);
        let p = args.iter().position(|a| a == "-p").expect("-p present");
        assert_eq!(args.get(p + 1).map(String::as_str), Some(pkg));
        assert!(!args.iter().any(|a| a == "engine"));
    }
    assert_eq!(
        Workload::DotKernel.cargo_args(pkg),
        ["bench", "--bench", "dot_kernel_bench", "-p", pkg]
    );
    assert_eq!(
        Workload::KnnProfile.cargo_args(pkg),
        ["bench", "--bench", "knn_profile_bench", "-p", pkg]
    );
    for w in [Workload::Feature128, Workload::Feature768] {
        assert_eq!(
            w.cargo_args(pkg),
            ["run", "--release", "-p", pkg, "--example", "feature_bench"]
        );
    }
}
