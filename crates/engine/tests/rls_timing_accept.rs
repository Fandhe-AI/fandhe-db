//! `benches/harness/rls_timing.rs`（Issue #2079。衝突有無で応答時間分布を比較するベンチ。
//! ポインタ: RLS-9・RLS-10）の回帰テスト。
//!
//! `rls_timing_bench.rs` は時間依存のためここでは実行せず、実測タイマーに依存しない契約
//! （env の fail-closed パース・SQL 事前生成と seed の対応・分位点・JSON の往復）のみを
//! `#[path]` で取り込み `cargo test` で検証する。プロセス共有状態は持たないが、
//! `tests/relational_p95_accept.rs` と同じく束ねず単独バイナリとして置く。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use std::collections::BTreeSet;
use std::time::Duration;

use harness::chip::parse_json;
use harness::relational_p95::rotate_arms;
use harness::rls_timing::*;

#[test]
fn parse_rounds_and_iters_boundaries() {
    assert_eq!(parse_rounds(None), Ok(DEFAULT_ROUNDS));
    assert_eq!(parse_rounds(Some("  ")), Ok(DEFAULT_ROUNDS));
    assert_eq!(parse_rounds(Some("5")), Ok(5));
    assert_eq!(parse_rounds(Some("50")), Ok(50));
    for bad in ["4", "51", "abc", "-1", "5.5"] {
        assert!(parse_rounds(Some(bad)).is_err(), "{bad}");
    }
    assert_eq!(parse_iters(Some("")), Ok(DEFAULT_ITERS));
    assert_eq!(parse_iters(Some("200")), Ok(200));
    assert_eq!(parse_iters(Some("20000")), Ok(20_000));
    for bad in ["199", "20001", "x", "1e3"] {
        assert!(parse_iters(Some(bad)).is_err(), "{bad}");
    }
}

#[test]
fn parse_shapes_is_exact_and_rejects_duplicates() {
    assert_eq!(parse_shapes(None), Ok(ALL_SHAPES.to_vec()));
    assert_eq!(
        parse_shapes(Some("normal,unique_violation")),
        Ok(vec![Shape::Normal, Shape::UniqueViolation])
    );
    for bad in ["Normal", "normal,normal", "nope", "normal,", " normal , x"] {
        assert!(parse_shapes(Some(bad)).is_err(), "{bad}");
    }
}

#[test]
fn github_actions_with_dedicated_is_contradictory() {
    assert!(check_env_consistency(true, true).is_err());
    assert!(check_env_consistency(true, false).is_ok());
    assert!(check_env_consistency(false, true).is_ok());
    assert!(check_env_consistency(false, false).is_ok());
}

#[test]
fn statements_are_unique_and_match_flood_seed() {
    let n = 50;
    let normal = build_statements(3, Shape::Normal, n);
    assert_eq!(normal.len(), n as usize);
    // op id が反復間で一意。
    let ops: BTreeSet<_> = normal
        .iter()
        .filter_map(|s| s.split("OPERATION_ID '").nth(1))
        .collect();
    assert_eq!(ops.len(), n as usize);
    // 各反復の id・UNIQUE 値は flooder の seed と 1 対 1 に対応する。
    let seeds = flood_extra_rows("t", n);
    for (i, sql) in normal.iter().enumerate() {
        let row = seeds
            .iter()
            .find(|r| r.table == "uniq" && r.id == NORMAL_ID_BASE + i as u64)
            .expect("seed row for iteration");
        assert_eq!(
            row.cols.first(),
            Some(&SeedCol::Text(normal_code(i as u32)))
        );
        assert!(sql.contains(&format!("VALUES ({}, '{}'", row.id, normal_code(i as u32))));
    }
    // 形状・ラウンドが違えば op id も違う。
    let a = build_statements(1, Shape::UniqueViolation, 1);
    let b = build_statements(2, Shape::UniqueViolation, 1);
    assert_ne!(a, b);
    // 衝突形状の新規 id は flooder の行 id と重ならない。
    let reject = build_statements(0, Shape::FkMissingParent, n);
    assert!(reject.iter().all(|s| s.contains("parent_id")));
    assert!(seeds.iter().all(|r| r.id < REJECT_ID_BASE));
}

#[test]
fn flood_rows_cover_foreign_only_parent() {
    let rows = flood_extra_rows("x", 0);
    assert!(rows
        .iter()
        .any(|r| r.table == "parents" && r.id == FOREIGN_ONLY_PARENT_ID));
    // 閲覧側の共通行は親 7 を持たない（baseline では親不在になる）。
    assert!(common_rows("v")
        .iter()
        .all(|r| !(r.table == "parents" && r.id == FOREIGN_ONLY_PARENT_ID)));
}

#[test]
fn classify_matches_positive_and_negative() {
    let rej = |c: &str| Outcome::Rejected {
        code: c.to_string(),
        message: String::new(),
    };
    assert!(classify_matches(Expected::Rejected("23505"), &rej("23505")));
    assert!(!classify_matches(
        Expected::Rejected("23505"),
        &rej("23503")
    ));
    assert!(!classify_matches(
        Expected::Rejected("23505"),
        &Outcome::Affected(1)
    ));
    assert!(classify_matches(
        Expected::Affected(1),
        &Outcome::Affected(1)
    ));
    assert!(!classify_matches(
        Expected::Affected(1),
        &Outcome::Affected(0)
    ));
    assert!(!classify_matches(Expected::Affected(1), &Outcome::Other));
}

#[test]
fn quantiles_use_nearest_rank() {
    let samples: Vec<Duration> = (1..=100u64).map(Duration::from_nanos).collect();
    let q = quantiles(&samples).expect("quantiles");
    assert_eq!((q.p50_ns, q.p95_ns, q.p99_ns, q.n), (50, 95, 99, 100));
    // 順不同でも結果は同じ。
    let mut rev = samples.clone();
    rev.reverse();
    assert_eq!(quantiles(&rev), Ok(q));
    assert_eq!(quantiles(&[]), Err(RlsTimingError::EmptySamples));
}

#[test]
fn arm_order_rotates_per_round() {
    assert_eq!(rotate_arms(0, ALL_ARMS.len()), vec![0, 1]);
    assert_eq!(rotate_arms(1, ALL_ARMS.len()), vec![1, 0]);
}

fn sample_rounds() -> Vec<Round> {
    (0..2)
        .map(|index| Round {
            index,
            cells: ALL_ARMS
                .iter()
                .map(|&arm| Cell {
                    shape: Shape::Normal,
                    arm,
                    mismatches: 0,
                    samples: (1..=10u64).map(Duration::from_nanos).collect(),
                })
                .collect(),
        })
        .collect()
}

#[test]
fn json_roundtrips_with_raw_samples_and_no_paths() {
    let env = EnvInfo {
        os: "linux".into(),
        arch: "x86_64".into(),
        logical_cpus: 8,
        isa: "Avx2".into(),
        loadavg: "0.1 0.2 0.3 1/2 3".into(),
        dedicated_env_attested: false,
        github_actions: false,
        git_commit: None,
    };
    let json = render_json(&env, 10, &[Shape::Normal], &sample_rounds()).expect("json");
    assert!(!json.contains("/home/") && !json.contains("/tmp/"));
    let v = parse_json(&json).expect("parse");
    assert_eq!(v.get("schema_version").and_then(|x| x.as_f64()), Some(1.0));
    let rounds = v.get("rounds").and_then(|x| x.as_array()).expect("rounds");
    assert_eq!(rounds.len(), 2);
    let cell = rounds[0]
        .get("cells")
        .and_then(|x| x.as_array())
        .and_then(|c| c.first())
        .expect("cell");
    let n_samples = cell
        .get("samples_ns")
        .and_then(|x| x.as_array())
        .map(|a| a.len());
    assert_eq!(n_samples, Some(10));
    let summary = v
        .get("summary")
        .and_then(|x| x.as_array())
        .expect("summary");
    assert_eq!(summary.len(), 2);
    // プールした件数 = 全ラウンドの生サンプル数。
    let pooled_n = summary[0]
        .get("pooled")
        .and_then(|p| p.get("n"))
        .and_then(|n| n.as_f64());
    assert_eq!(pooled_n, Some(20.0));
}
// ---------------------------------------------------------------------
// 判定ロジック（Issue #2080）。合成データで時間非依存に検証する。
// ---------------------------------------------------------------------

fn q_of(p50: u64, p95: u64, p99: u64) -> Quantiles {
    Quantiles {
        p50_ns: p50,
        p95_ns: p95,
        p99_ns: p99,
        n: 100,
    }
}

fn row(shape: Shape, arm: Arm, pooled: Quantiles, rounds: &[Quantiles]) -> SummaryRow {
    SummaryRow {
        shape,
        arm,
        pooled,
        per_round: rounds
            .iter()
            .enumerate()
            .map(|(i, q)| (i as u32, *q))
            .collect(),
    }
}

fn flat(q: Quantiles) -> Vec<Quantiles> {
    vec![q; 5]
}

fn pair(base: Quantiles, flood: Quantiles) -> Vec<SummaryRow> {
    vec![
        row(Shape::Normal, Arm::Baseline, base, &flat(base)),
        row(Shape::Normal, Arm::Flooded, flood, &flat(flood)),
    ]
}

#[test]
fn relative_diff_is_symmetric_and_zero_safe() {
    assert_eq!(relative_diff(0, 0), 0.0);
    assert_eq!(relative_diff(90, 100), relative_diff(100, 90));
    assert_eq!(relative_diff(90, 100), 0.1);
    assert_eq!(relative_diff(0, 5), 1.0);
}

#[test]
fn aa_band_is_full_width_over_min_and_infinite_on_zero() {
    assert_eq!(aa_band(&[100, 120, 110]), 0.2);
    assert_eq!(aa_band(&[100, 100]), 0.0);
    assert_eq!(aa_band(&[0, 10]), f64::INFINITY);
    assert_eq!(aa_band(&[]), f64::INFINITY);
}

#[test]
fn identical_distributions_are_indistinguishable() {
    let q = q_of(1000, 2000, 3000);
    let j = judge_rows(&pair(q, q)).expect("judge");
    assert_eq!(j[0].verdict, Verdict::Indistinguishable);
    assert_eq!(overall(&j), Verdict::Indistinguishable);
    assert!(render_overall_line(&j).starts_with("rls_timing overall=pass"));
}

#[test]
fn slow_p95_only_is_distinguishable_with_reason() {
    let base = q_of(1000, 2000, 3000);
    let flood = q_of(1000, 3000, 3000);
    let j = judge_rows(&pair(base, flood)).expect("judge");
    assert_eq!(j[0].verdict, Verdict::Distinguishable);
    let p95 = j[0]
        .quantiles
        .iter()
        .find(|x| x.quantile == Quantile::P95)
        .expect("p95");
    assert_eq!(p95.verdict, Verdict::Distinguishable);
    assert_eq!(overall(&j), Verdict::Distinguishable);
    let line = render_overall_line(&j);
    assert!(line.starts_with("rls_timing overall=fail") && line.contains("normal:p95:"));
}

#[test]
fn boundary_delta_equal_to_band_is_indistinguishable() {
    // delta = 10/100 = 0.10 = 固定帯（p95）。厳密に超えた場合だけ区別可能。
    let at = judge_rows(&pair(q_of(100, 90, 100), q_of(100, 100, 100))).expect("judge");
    assert_eq!(at[0].verdict, Verdict::Indistinguishable);
    let over = judge_rows(&pair(q_of(100, 89, 100), q_of(100, 100, 100))).expect("judge");
    assert_eq!(over[0].verdict, Verdict::Distinguishable);
}

#[test]
fn wide_aa_band_widens_threshold_and_cap_makes_it_inconclusive() {
    // aa(p95) = 0.2 <= cap 0.5 のため許容幅は max(0.10, 0.2) = 0.2。delta 0.15 は区別不能。
    let base_rounds = vec![q_of(1000, 1000, 3000), q_of(1000, 1200, 3000)];
    let rows = vec![
        row(
            Shape::Normal,
            Arm::Baseline,
            q_of(1000, 1000, 3000),
            &base_rounds,
        ),
        row(
            Shape::Normal,
            Arm::Flooded,
            q_of(1000, 850, 3000),
            &flat(q_of(1000, 850, 3000)),
        ),
    ];
    let j = judge_rows(&rows).expect("judge");
    assert_eq!(j[0].verdict, Verdict::Indistinguishable);
    // aa = 1.0 > cap 0.5 は判定不能。
    let noisy = vec![q_of(1000, 1000, 3000), q_of(1000, 2000, 3000)];
    let rows = vec![
        row(Shape::Normal, Arm::Baseline, q_of(1000, 1000, 3000), &noisy),
        row(
            Shape::Normal,
            Arm::Flooded,
            q_of(1000, 1000, 3000),
            &flat(q_of(1000, 1000, 3000)),
        ),
    ];
    let j = judge_rows(&rows).expect("judge");
    assert_eq!(j[0].verdict, Verdict::Inconclusive);
}

#[test]
fn inconclusive_beats_distinguishable_and_overall_needs_all_pass() {
    // p50 は判定不能（aa 超過）、p95 は区別可能 → 形状は Inconclusive。
    let noisy = vec![q_of(1000, 2000, 3000), q_of(2000, 2000, 3000)];
    let rows = vec![
        row(Shape::Normal, Arm::Baseline, q_of(1000, 2000, 3000), &noisy),
        row(
            Shape::Normal,
            Arm::Flooded,
            q_of(1000, 4000, 3000),
            &flat(q_of(1000, 4000, 3000)),
        ),
    ];
    let j = judge_rows(&rows).expect("judge");
    assert_eq!(j[0].verdict, Verdict::Inconclusive);
    assert_eq!(overall(&j), Verdict::Inconclusive);
    // 形状が無ければ pass にしない。
    assert_eq!(overall(&[]), Verdict::Inconclusive);
    // 1 形状でも区別可能なら全体は pass にならない。
    let q = q_of(1000, 2000, 3000);
    let mut rows = pair(q, q);
    rows.push(row(Shape::UniqueViolation, Arm::Baseline, q, &flat(q)));
    rows.push(row(
        Shape::UniqueViolation,
        Arm::Flooded,
        q_of(1000, 9000, 3000),
        &flat(q_of(1000, 9000, 3000)),
    ));
    let j = judge_rows(&rows).expect("judge");
    assert_eq!(overall(&j), Verdict::Distinguishable);
}

#[test]
fn judge_rows_rejects_missing_arm() {
    let q = q_of(1, 2, 3);
    let rows = vec![row(Shape::Normal, Arm::Baseline, q, &flat(q))];
    assert!(matches!(
        judge_rows(&rows),
        Err(RlsTimingError::MissingSummary(_))
    ));
}

fn test_env() -> EnvInfo {
    EnvInfo {
        os: "linux".into(),
        arch: "x86_64".into(),
        logical_cpus: 8,
        isa: "Avx2".into(),
        loadavg: "0".into(),
        dedicated_env_attested: false,
        github_actions: false,
        git_commit: None,
    }
}

#[test]
fn raw_json_roundtrip_matches_in_memory_summary() {
    let rounds = sample_rounds();
    let json = render_json(&test_env(), 10, &[Shape::Normal], &rounds).expect("json");
    let (shapes, back) = parse_raw_rounds(&json).expect("parse raw");
    assert_eq!(shapes, vec![Shape::Normal]);
    let a = summarize(&rounds, &[Shape::Normal]).expect("a");
    let b = summarize(&back, &shapes).expect("b");
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!((x.shape, x.arm, x.pooled), (y.shape, y.arm, y.pooled));
        assert_eq!(x.per_round, y.per_round);
    }
}

#[test]
fn parse_raw_rounds_fails_closed() {
    let json = render_json(&test_env(), 10, &[Shape::Normal], &sample_rounds()).expect("json");
    let bad = |from: &str, to: &str| {
        assert!(json.contains(from), "{from}");
        assert!(
            parse_raw_rounds(&json.replacen(from, to, 1)).is_err(),
            "{to}"
        );
    };
    bad("\"schema_version\":1", "\"schema_version\":2");
    bad("\"quantile_method\"", "\"quantile_methodx\"");
    bad("\"samples_ns\":[1,", "\"samples_ns\":[-1,");
    bad("\"samples_ns\":[1,", "\"samples_ns\":[1.5,");
    bad("\"samples_ns\":[1,", "\"samples_ns\":[\"x\",");
    bad("\"shape\":\"normal\"", "\"shape\":\"bogus\"");
    bad("\"arm\":\"baseline\"", "\"arm\":\"bogus\"");
    bad("\"mismatches\":0", "\"mismatches\":1");
    // 同じ arm が 2 回 → 重複セル（かつ flooded セル欠落）。
    bad("\"arm\":\"flooded\"", "\"arm\":\"baseline\"");
    // 宣言件数と実データの不一致（データ欠落）は拒否される。
    let declared = format!("\"rounds\":{},", sample_rounds().len());
    bad(&declared, "\"rounds\":99,");
    bad("\"measured\":10", "\"measured\":11");
    assert!(parse_raw_rounds("").is_err());
    assert!(parse_raw_rounds("{").is_err());
    assert!(parse_raw_rounds("{\"schema_version\":1}").is_err());
    let empty = json.replacen(
        "\"samples_ns\":[1,2,3,4,5,6,7,8,9,10]",
        "\"samples_ns\":[]",
        1,
    );
    assert!(parse_raw_rounds(&empty).is_err());
}

#[test]
fn raw_input_limit_covers_max_measurement_output() {
    // 最大設定の出力量（ROUNDS=50・ITERS=20000・全形状・両 arm）が既定の 4 MiB を超えても、
    // 再判定の有限上限が最悪桁数のサンプル列を収められることを固定する。
    const { assert!(RAW_MAX_INPUT_BYTES > 4 * 1024 * 1024) };
    assert_eq!(MAX_RAW_SAMPLES, 50 * 20_000 * 6);
    const { assert!(RAW_MAX_INPUT_BYTES >= MAX_RAW_SAMPLES * "18446744073709551615,".len()) };
    // 4 MiB 超の入力が上限検査で拒否されず、構文／検証段（Err でも可）まで進む。
    let big = format!("{{\"x\":\"{}\"}}", "a".repeat(5 * 1024 * 1024));
    let err = parse_raw_rounds(&big).expect_err("invalid raw");
    assert!(!format!("{err:?}").contains("exceeds"), "{err:?}");
    // 有限上限を超える入力は拒否される。
    let huge = "a".repeat(RAW_MAX_INPUT_BYTES + 1);
    let err = parse_raw_rounds(&huge).expect_err("too large");
    assert!(format!("{err:?}").contains("exceeds"), "{err:?}");
}
