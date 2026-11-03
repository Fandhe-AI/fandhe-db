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
    // pgvector（float8 順序）と同じく NaN は全数値の後ろ、NaN 同士は id 昇順。
    // `search_order` を参照しない独立実装として保つ。
    v.sort_by(|a, b| match (a.1.is_nan(), b.1.is_nan()) {
        (true, true) => a.0.cmp(&b.0),
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)),
    });
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

#[test]
fn zero_norm_rows_sort_last_like_pgvector() {
    for dim in [3usize, 8, 17] {
        let rows = 23usize;
        let mut m = fixture(dim, rows, 0, 77 + dim as u64);
        let zero_idx = [0usize, 5, 6, 22];
        for &i in &zero_idx {
            m[i * dim..(i + 1) * dim].fill(0.0);
        }
        let ids: Vec<u64> = (100..100 + rows as u64).collect();
        let q: Vec<f32> = (0..dim).map(|i| 0.3 + i as f32 * 0.1).collect();
        let finite = rows - zero_idx.len();

        let got = run(&ids, &m, dim, &q, rows, None);
        let full = got.clone();
        let want = reference(&ids, &m, dim, &q);
        assert_eq!(got.len(), rows);
        for (g, w) in got.iter().zip(&want).take(finite) {
            assert_eq!(g.0, w.0, "dim={dim}");
            assert!((g.1 - w.1).abs() < TOL);
        }
        let tail: Vec<u64> = got[finite..].iter().map(|h| h.0).collect();
        let expect_tail: Vec<u64> = zero_idx.iter().map(|&i| ids[i]).collect();
        assert_eq!(tail, expect_tail, "dim={dim}");
        assert!(got[finite..].iter().all(|h| h.1.is_nan()));

        // 有限行 + 1 件を要求すると NaN 行は id 最小の 1 件だけ入る。
        let got = run(&ids, &m, dim, &q, finite + 1, None);
        assert_eq!(got.len(), finite + 1);
        assert_eq!(got[finite].0, ids[zero_idx[0]]);
        assert!(got[finite].1.is_nan());

        // C2（行ノルム²を外から渡す）でも同じ。
        let norms = row_norms_sq(&m, dim as u32).unwrap();
        let c2 = run(&ids, &m, dim, &q, rows, Some(&norms));
        let ids_of = |v: &[(u64, f64)]| v.iter().map(|h| h.0).collect::<Vec<_>>();
        assert_eq!(ids_of(&c2), ids_of(&full), "dim={dim}");
    }
}

#[test]
fn zero_norm_query_returns_rows_in_id_order() {
    let dim = 4usize;
    let rows = 9usize;
    let m = fixture(dim, rows, 0, 5);
    let ids: Vec<u64> = (1..=rows as u64).collect();
    let q = vec![0.0f32; dim];
    for k in [1usize, 4, 9, 20] {
        let got = run(&ids, &m, dim, &q, k, None);
        assert_eq!(got.len(), k.min(rows));
        let want: Vec<u64> = (1..=got.len() as u64).collect();
        assert_eq!(got.iter().map(|h| h.0).collect::<Vec<_>>(), want);
        assert!(got.iter().all(|h| h.1.is_nan()));
    }
}
