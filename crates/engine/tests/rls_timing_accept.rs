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
