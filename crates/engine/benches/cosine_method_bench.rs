//! コサイン距離の実装方式比較（Issue #1986。ADR は `docs/design/cosine-distance-method.md`）
//! の実測入口。`make bench-cosine-method` からの手動実行専用。
//!
//! # 位置づけ
//!
//! - 比較対象: 方式 A（正規化済み前提の `1 - dot`）・B′（原値と正規化値を両方持つ）・
//!   C1（dot・`‖x‖²`・`‖q‖²` の 1 パス融合）・C2（行ノルム² を事前計算）。いずれも
//!   `harness::cosine_method` の試作で、production のカーネル（`engine::kernel`・
//!   `engine::hnsw`・`gpu_batch`）には触れない。production 実装は #1987 が担う。
//! - spec 由来の pass/fail 閾値を持たない情報提供専用ベンチ。`.github/workflows/*` へ
//!   配線せず、`GITHUB_ACTIONS` 下では拒否する（fail-closed。他の手動ベンチと同一方針）。
//! - 共有 QEMU 環境の時間値は `docs/design/benchmark-judgement-policy.md` §5・§6 により
//!   採否根拠にならない。ADR は構造的理由で判定し、本ベンチの時間値は参考値として添える。
//!
//! # 出力
//!
//! 1. 精度: fixture × dim × 方式ごとに、pgvector 式エミュレーションおよび f64 厳密値との
//!    `|Δd|`（最大・平均）、Top-10 の一致率、ゼロノルム行の NaN 不一致数。C1 と C2 が
//!    ビット一致しない場合は実測値を出さず非 0 で終了する。
//! 2. 検索コスト: 方式ごとの ns/行（ラウンドロビン交互。min と median 併記）。
//! 3. 書き込みコスト: B′ の正規化複製と C2 のノルム² 計算の ns/行。
//! 4. `EnvReport` の環境情報。
//!
//! # opt-in
//!
//! `BENCH_COSINE_METHOD_EMIT_SQL=1` で fixture を pgvector リテラルの SQL として stdout に、
//! 期待値（エミュレーション・厳密値・C1・A の順）を stderr に出し、計測は行わずに終了する（実 pgvector との
//! 照合用。決定的 fixture の数値リテラルだけで構成し、外部入力は連結しない）。

#[allow(dead_code)]
mod harness;

use std::hint::black_box;
use std::time::{Duration, Instant};

use harness::cosine_method::{
    c1_fused_distance, c2_distance, check_c1_c2_bit_identical, dot_lanes, error_stats,
    exact_cosine_distance, generate_fixture, method_a_distance, norm_sq_lanes, normalized_copy,
    parse_flag_env, pgvector_cosine_distance, refuse_under_github_actions, top_k, topk_agreement,
    vector_literal, Fixture, FixtureKind,
};
use harness::env_report::EnvReport;
use harness::stats::summarize;

use engine::isa;

const DIMS: [usize; 3] = [128, 768, 1536];
/// 精度評価・計測に使う行数（固定上限。fixture の最大メモリは 2000 × 1536 × 4 B 程度）。
const ROWS: usize = 2000;
const TOP_K: usize = 10;
/// 計測ラウンド数（各ラウンドで全 arm を 1 パスずつ回す）。
const ROUNDS: usize = 9;
const SEED: u64 = 0x1986_C051;

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    refuse_under_github_actions(std::env::var_os("GITHUB_ACTIONS").is_some())?;
    let emit_sql = parse_flag_env(
        std::env::var("BENCH_COSINE_METHOD_EMIT_SQL")
            .ok()
            .as_deref(),
    )?;
    if emit_sql {
        return emit_sql_fixtures();
    }

    println!(
        "{}",
        EnvReport::capture(format!("{:?}", isa::current().isa()))
    );
    println!("rows={ROWS} top_k={TOP_K} rounds={ROUNDS} (timing is reference only)");

    println!("\n== accuracy (d = cosine distance; delta in absolute value) ==");
    for kind in FixtureKind::ALL {
        for dim in DIMS {
            let f = generate_fixture(kind, SEED ^ dim as u64, dim, ROWS)?;
            // fail-closed: C1 と C2 がビット一致しなければ実測値を出さない。
            if let Err(row) = check_c1_c2_bit_identical(&f) {
                return Err(format!(
                    "C1/C2 bit mismatch at fixture={} dim={dim} row={row}",
                    kind.name()
                )
                .into());
            }
            report_accuracy(kind, &f);
        }
    }

    println!("\n== search cost (ns/row, round-robin; reference only) ==");
    for dim in DIMS {
        let f = generate_fixture(FixtureKind::Scattered, SEED ^ dim as u64, dim, ROWS)?;
        report_search_cost(&f);
    }

    println!("\n== write cost (ns/row) ==");
    for dim in DIMS {
        let f = generate_fixture(FixtureKind::Scattered, SEED ^ dim as u64, dim, ROWS)?;
        report_write_cost(&f);
    }
    Ok(())
}

/// 各方式の距離列を作る。
struct Arms {
    emu: Vec<f64>,
    exact: Vec<f64>,
    a: Vec<f64>,
    b_prime: Vec<f64>,
    c1: Vec<f64>,
    /// C2 を production の `isa::dot` で計算したもの（8 レーン試作とはビット一致しない）。
    c2_isa: Vec<f64>,
}

fn compute_arms(f: &Fixture) -> Arms {
    let qn = normalized_copy(&f.query);
    let emu = f
        .iter_rows()
        .map(|r| pgvector_cosine_distance(r, &f.query))
        .collect();
    let exact = f
        .iter_rows()
        .map(|r| exact_cosine_distance(r, &f.query))
        .collect();
    let a = f
        .iter_rows()
        .map(|r| method_a_distance(r, &f.query))
        .collect();
    let b_prime = f
        .iter_rows()
        .map(|r| method_a_distance(&normalized_copy(r), &qn))
        .collect();
    let c1 = f
        .iter_rows()
        .map(|r| c1_fused_distance(r, &f.query))
        .collect();
    let kernel = isa::current();
    let q_norm_sq = norm_sq_lanes(&f.query);
    let c2_isa = f
        .iter_rows()
        .map(|r| c2_distance(kernel.dot(r, &f.query), norm_sq_lanes(r), q_norm_sq))
        .collect();
    Arms {
        emu,
        exact,
        a,
        b_prime,
        c1,
        c2_isa,
    }
}

fn report_accuracy(kind: FixtureKind, f: &Fixture) {
    let arms = compute_arms(f);
    let ref_top = top_k(&arms.emu, TOP_K);
    let min_d = arms
        .emu
        .iter()
        .copied()
        .filter(|d| d.is_finite())
        .fold(f64::INFINITY, f64::min);
    let rows = [
        ("emu(f32 seq)", &arms.emu),
        ("A", &arms.a),
        ("B'", &arms.b_prime),
        ("C1/C2(lanes)", &arms.c1),
        ("C2(isa dot)", &arms.c2_isa),
    ];
    println!(
        "fixture={} dim={} min_d(emu)={:.3e}",
        kind.name(),
        f.dim,
        min_d
    );
    for (name, d) in rows {
        let vs_emu = error_stats(d, &arms.emu);
        let vs_exact = error_stats(d, &arms.exact);
        let (overlap, same_order) = topk_agreement(&top_k(d, TOP_K), &ref_top);
        println!(
            "  {name:<13} max|d-emu|={:.3e} mean={:.3e} max|d-exact|={:.3e} nan_mismatch={} top{TOP_K}_overlap={:.2} order_same={}",
            vs_emu.max_abs, vs_emu.mean_abs, vs_exact.max_abs, vs_emu.nan_mismatch, overlap, same_order
        );
    }
}

/// 1 パスの総和を返す（最適化で消されないよう `black_box` を通す）。
fn pass<F: FnMut(&[f32]) -> f64>(f: &Fixture, mut op: F) -> f64 {
    let mut s = 0f64;
    for r in f.iter_rows() {
        s += op(r);
    }
    black_box(s)
}

fn report_search_cost(f: &Fixture) {
    let q = &f.query;
    let qn = normalized_copy(q);
    let q_norm_sq = norm_sq_lanes(q);
    let normed: Vec<Vec<f32>> = f.iter_rows().map(normalized_copy).collect();
    let row_norms: Vec<f32> = f.iter_rows().map(norm_sq_lanes).collect();
    let kernel = isa::current();
    let n = f.n_rows();

    let names = [
        "A(isa dot)",
        "B'(isa dot)",
        "C1 fused",
        "C2 lanes",
        "C2 isa dot",
        "emu f32 seq",
    ];
    let mut samples: Vec<Vec<Duration>> = vec![Vec::new(); names.len()];
    for round in 0..ROUNDS + 1 {
        for k in 0..names.len() {
            // 先頭ラウンドは warmup として捨て、以降は開始 arm をずらして順序バイアスを避ける。
            let arm = (k + round) % names.len();
            let t = Instant::now();
            match arm {
                0 => {
                    pass(f, |r| 1.0 - kernel.dot(r, q) as f64);
                }
                1 => {
                    let mut s = 0f64;
                    for r in &normed {
                        s += 1.0 - kernel.dot(r, &qn) as f64;
                    }
                    black_box(s);
                }
                2 => {
                    pass(f, |r| c1_fused_distance(r, q));
                }
                3 => {
                    let mut i = 0;
                    pass(f, |r| {
                        let d = c2_distance(dot_lanes(r, q), row_norms[i], q_norm_sq);
                        i += 1;
                        d
                    });
                }
                4 => {
                    let mut i = 0;
                    pass(f, |r| {
                        let d = c2_distance(kernel.dot(r, q), row_norms[i], q_norm_sq);
                        i += 1;
                        d
                    });
                }
                _ => {
                    pass(f, |r| pgvector_cosine_distance(r, q));
                }
            }
            let el = t.elapsed();
            if round > 0 {
                samples[arm].push(el);
            }
        }
    }
    println!("dim={} rows={n}", f.dim);
    for (name, s) in names.iter().zip(&samples) {
        let min = s.iter().min().copied().unwrap_or_default();
        let med = summarize(s).map(|x| x.median).unwrap_or_default();
        println!(
            "  {name:<12} min={:.2} ns/row median={:.2} ns/row",
            min.as_secs_f64() * 1e9 / n as f64,
            med.as_secs_f64() * 1e9 / n as f64
        );
    }
}

fn report_write_cost(f: &Fixture) {
    let n = f.n_rows();
    let mut b_samples = Vec::new();
    let mut c2_samples = Vec::new();
    for _ in 0..ROUNDS {
        let t = Instant::now();
        let mut keep = Vec::with_capacity(n);
        for r in f.iter_rows() {
            keep.push(normalized_copy(r));
        }
        black_box(&keep);
        b_samples.push(t.elapsed());
        drop(keep);

        let t = Instant::now();
        let mut s = 0f32;
        for r in f.iter_rows() {
            s += norm_sq_lanes(r);
        }
        black_box(s);
        c2_samples.push(t.elapsed());
    }
    let per = |s: &[Duration]| {
        let min = s.iter().min().copied().unwrap_or_default();
        let med = summarize(s).map(|x| x.median).unwrap_or_default();
        (
            min.as_secs_f64() * 1e9 / n as f64,
            med.as_secs_f64() * 1e9 / n as f64,
        )
    };
    let (bmin, bmed) = per(&b_samples);
    let (cmin, cmed) = per(&c2_samples);
    println!(
        "dim={} B' normalize+copy: min={bmin:.1} median={bmed:.1} ns/row | C2 norm_sq: min={cmin:.1} median={cmed:.1} ns/row | extra bytes/row: B'={} C2=4",
        f.dim,
        f.dim * 4
    );
}

/// 実 pgvector との照合用に fixture を SQL として stdout、期待値を stderr に出す。
fn emit_sql_fixtures() -> Result<(), Box<dyn std::error::Error>> {
    const EMIT_DIM: usize = 128;
    const EMIT_ROWS: usize = 200;
    let kernel = isa::current();
    println!("CREATE EXTENSION IF NOT EXISTS vector;");
    for kind in FixtureKind::ALL {
        let f = generate_fixture(kind, SEED ^ EMIT_DIM as u64, EMIT_DIM, EMIT_ROWS)?;
        let t = format!("t_{}", kind.name());
        let q_norm_sq = norm_sq_lanes(&f.query);
        println!("DROP TABLE IF EXISTS {t};");
        println!("CREATE TABLE {t} (id int PRIMARY KEY, v vector({EMIT_DIM}));");
        for (i, r) in f.iter_rows().enumerate() {
            println!("INSERT INTO {t} VALUES ({i}, '{}');", vector_literal(r));
        }
        println!(
            "SELECT id, v <=> '{}' FROM {t} ORDER BY id;",
            vector_literal(&f.query)
        );
        for (i, r) in f.iter_rows().enumerate() {
            eprintln!(
                "EXPECT\t{t}\t{i}\t{:e}\t{:e}\t{:e}\t{:e}\t{:e}",
                pgvector_cosine_distance(r, &f.query),
                exact_cosine_distance(r, &f.query),
                c1_fused_distance(r, &f.query),
                method_a_distance(r, &f.query),
                c2_distance(kernel.dot(r, &f.query), norm_sq_lanes(r), q_norm_sq)
            );
        }
    }
    Ok(())
}
