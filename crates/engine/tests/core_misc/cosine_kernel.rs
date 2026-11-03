//! コサイン距離カーネル `kernel::cosine_top_k` の公開 API 経由の照合テスト（Issue #1987。
//! ポインタ: SQL-33・TASK-237／238）。独立に書いた f64 の brute-force 参照と Top-k を比べる。
//! 許容差 1e-6 は #1986（Proposed）の提案値に基づく暫定値で、規範化は spec 側の課題。

use engine::isa::dot_scalar;
use engine::kernel::{cosine_top_k, row_norms_sq, CosineSearchInput};

const TOL: f64 = 1e-6;

struct XorShift(u64);
impl XorShift {
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

/// kind 0: 未正規化（大きさ 1e-3〜1e3）、1: 正規化済み、2: ほぼ重複行を含む。
fn fixture(dim: usize, rows: usize, kind: u8, seed: u64) -> Vec<f32> {
    let mut rng = XorShift(seed | 1);
    let mut out: Vec<f32> = Vec::with_capacity(dim * rows);
    for r in 0..rows {
        let mut v: Vec<f32> = (0..dim).map(|_| rng.next_f32() + 0.01).collect();
        if kind == 2 && r > 0 && r % 5 == 0 {
            let prev = out[(r - 1) * dim..r * dim].to_vec();
            v = prev
                .iter()
                .map(|x| x * (1.0 + 1e-6 * rng.next_f32()))
                .collect();
        }
        match kind {
            0 => {
                let scale = 10f32.powf(rng.next_f32() * 3.0);
                v.iter_mut().for_each(|x| *x *= scale);
            }
            1 => {
                let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                v.iter_mut().for_each(|x| *x /= n);
            }
            _ => {}
        }
        out.extend(v);
    }
    out
}

fn ref_dist(a: &[f32], b: &[f32]) -> f64 {
    let (mut d, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        d += f64::from(*x) * f64::from(*y);
        na += f64::from(*x) * f64::from(*x);
        nb += f64::from(*y) * f64::from(*y);
    }
    1.0 - (d / (na * nb).sqrt()).clamp(-1.0, 1.0)
}

fn reference(ids: &[u64], m: &[f32], dim: usize, q: &[f32]) -> Vec<(u64, f64)> {
    let mut v: Vec<(u64, f64)> = ids
        .iter()
        .zip(m.chunks_exact(dim))
        .map(|(&id, row)| (id, ref_dist(row, q)))
        .collect();
    v.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    v
}

fn run(
    ids: &[u64],
    m: &[f32],
    dim: usize,
    q: &[f32],
    k: usize,
    norms: Option<&[f32]>,
) -> Vec<(u64, f64)> {
    cosine_top_k(CosineSearchInput {
        ids,
        vectors: m,
        row_norms_sq: norms,
        dim: dim as u32,
        query: q,
        k,
    })
    .unwrap()
    .into_iter()
    .map(|h| (h.id, h.distance))
    .collect()
}

#[test]
fn top_k_matches_brute_force_reference() {
    for &dim in &[1usize, 3, 7, 8, 16, 17, 128, 768] {
        for kind in 0..3u8 {
            let rows = 53;
            let m = fixture(dim, rows, kind, 0x9e37 + dim as u64 * 31 + kind as u64);
            let q = fixture(dim, 1, 1, 0x1234 + dim as u64);
            let ids: Vec<u64> = (0..rows as u64).map(|i| i * 2 + 1).collect();
            let k = 10;
            let got = run(&ids, &m, dim, &q, k, None);
            let want = reference(&ids, &m, dim, &q);
            assert_eq!(got.len(), k);
            for (i, (id, d)) in got.iter().enumerate() {
                // 各位置の距離が参照と許容差内（同点近傍の入れ替わりは距離差が許容差以下）。
                assert!(
                    (d - want[i].1).abs() <= TOL,
                    "dim={dim} kind={kind} pos={i}"
                );
                // 返った行自体の距離も参照と一致すること。
                let row_idx = ids.iter().position(|x| x == id).unwrap();
                let rd = ref_dist(&m[row_idx * dim..(row_idx + 1) * dim], &q);
                assert!((d - rd).abs() <= TOL, "dim={dim} kind={kind} id={id}");
            }
            assert!(got.windows(2).all(|w| w[0].1 <= w[1].1));
        }
    }
}

#[test]
fn c1_and_c2_are_bit_identical() {
    let dim = 17;
    let m = fixture(dim, 37, 0, 77);
    let q = fixture(dim, 1, 0, 78);
    let ids: Vec<u64> = (0..37).collect();
    let norms = row_norms_sq(&m, dim as u32).unwrap();
    let c1 = run(&ids, &m, dim, &q, 37, None);
    let c2 = run(&ids, &m, dim, &q, 37, Some(&norms));
    assert_eq!(c1.len(), c2.len());
    for (a, b) in c1.iter().zip(&c2) {
        assert_eq!(a.0, b.0);
        assert_eq!(a.1.to_bits(), b.1.to_bits());
    }
}

#[test]
fn matches_scalar_isa_reference_within_tolerance() {
    for &dim in &[7usize, 16, 128] {
        let m = fixture(dim, 29, 0, 5);
        let q = fixture(dim, 1, 0, 6);
        let ids: Vec<u64> = (0..29).collect();
        for (id, d) in run(&ids, &m, dim, &q, 29, None) {
            let row = &m[id as usize * dim..(id as usize + 1) * dim];
            let s = f64::from(dot_scalar(row, &q))
                / (f64::from(dot_scalar(row, row)) * f64::from(dot_scalar(&q, &q))).sqrt();
            assert!((d - (1.0 - s.clamp(-1.0, 1.0))).abs() <= TOL);
        }
    }
}

#[test]
fn distance_order_differs_from_inner_product_order_for_unnormalized_rows() {
    // 旧契約（内積降順）との違いを固定する対照: 大きさの大きい直交寄りの行が内積では勝つ。
    let m = [100.0f32, 100.0, 1.0, 0.0];
    let q = [1.0f32, 0.0];
    let ids = [1u64, 2];
    let got = run(&ids, &m, 2, &q, 1, None);
    assert_eq!(got[0].0, 2);
    assert!(dot_scalar(&m[..2], &q) > dot_scalar(&m[2..], &q));
}
