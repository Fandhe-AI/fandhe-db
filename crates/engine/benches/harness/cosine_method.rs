//! コサイン距離の実装方式比較（Issue #1986。ADR は `docs/design/cosine-distance-method.md`）
//! が使う、時間非依存の fixture 生成・各方式の参照実装・誤差集計。
//!
//! `benches/cosine_method_bench.rs`（時間依存・手動実行専用・`make ci` 対象外）から呼ばれる。
//! 本モジュールは `std` と [`super::rng`] のみに依存し、production のカーネルは変更しない。
//! 方式 A/B′/C1/C2 は「ADR の比較用の試作」であり、production コードへは #1987 が別途実装する。
//!
//! 仕様ポインタ: SQL-33・TASK-237／238。pgvector の距離式は手法名のみを参照し、
//! 外部コードは転記しない（式の手順は ADR §7）。
//!
//! # 方式の累積規約
//!
//! - pgvector 式エミュレーション（[`pgvector_cosine_distance`]）: `dot`・`na`・`nb` を
//!   f32 で逐次累積し、f64 の `dot / sqrt(na * nb)` を `[-1, 1]` へクランプして
//!   `1 - s` を f64 で返す。ゼロノルムは NaN（#2240 と整合）。
//! - C1/C2 は同じ 8 レーン構造・同じ縮約順なので、C2 が `‖x‖²` を C1 と同じ累積順で
//!   キャッシュすれば 2 方式はビット一致する（[`check_c1_c2_bit_identical`]）。

use super::rng::DeterministicRng;

/// 試作カーネルのレーン数（`isa.rs` の `dot_lanes` と同じ流儀の safe 実装）。
pub const LANES: usize = 8;

/// 1 fixture あたりの最大行数（無制限確保禁止。coding-rust.md）。
pub const MAX_ROWS: usize = 100_000;

/// 1 行あたりの最大次元数（固定上限）。
pub const MAX_DIM: usize = 4096;

/// fixture の種別。ADR の比較軸（正規化済み／未正規化／ほぼ重複／ゼロノルム）に対応する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureKind {
    /// L2 正規化済みの行（方式 A が成立する前提）。
    Normalized,
    /// ノルムを 1e-3〜1e3 に散らした未正規化行。
    Scattered,
    /// クエリに 1e-6 程度まで近い行（`1 - s` の桁落ちを見る）。
    NearDuplicate,
    /// 未正規化行に少数のゼロノルム行を混ぜたもの。
    WithZero,
}

impl FixtureKind {
    /// 全種別（出力順）。
    pub const ALL: [FixtureKind; 4] = [
        FixtureKind::Normalized,
        FixtureKind::Scattered,
        FixtureKind::NearDuplicate,
        FixtureKind::WithZero,
    ];

    /// 表示用の英語名。
    pub fn name(self) -> &'static str {
        match self {
            FixtureKind::Normalized => "normalized",
            FixtureKind::Scattered => "scattered",
            FixtureKind::NearDuplicate => "near_dup",
            FixtureKind::WithZero => "with_zero",
        }
    }
}

/// 生成した fixture（行は連続配置）。
pub struct Fixture {
    pub dim: usize,
    pub rows: Vec<f32>,
    pub query: Vec<f32>,
}

impl Fixture {
    /// 行数。
    pub fn n_rows(&self) -> usize {
        self.rows.len().checked_div(self.dim).unwrap_or(0)
    }

    /// 行のイテレータ。
    pub fn iter_rows(&self) -> impl Iterator<Item = &[f32]> {
        self.rows.chunks_exact(self.dim.max(1))
    }
}

/// 本モジュールのエラー型。
#[derive(Debug, PartialEq)]
pub enum CosineMethodError {
    /// `GITHUB_ACTIONS` 下での実行。
    RefusedUnderGitHubActions,
    /// 行数・次元が固定上限を超えた、または 0。
    BadShape,
    /// 環境変数の値が不正（fail-closed）。
    BadEnv(String),
}

impl std::fmt::Display for CosineMethodError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RefusedUnderGitHubActions => {
                write!(
                    f,
                    "refused: cosine_method_bench must not run under GITHUB_ACTIONS"
                )
            }
            Self::BadShape => write!(f, "rows/dim out of the allowed range"),
            Self::BadEnv(v) => write!(f, "invalid env value: {v:?} (expected unset, 0 or 1)"),
        }
    }
}

impl std::error::Error for CosineMethodError {}

/// `GITHUB_ACTIONS` 下の実行を拒否する（他の手動ベンチと同一方針）。
pub fn refuse_under_github_actions(under: bool) -> Result<(), CosineMethodError> {
    if under {
        return Err(CosineMethodError::RefusedUnderGitHubActions);
    }
    Ok(())
}

/// opt-in の env（未設定・`0` は false、`1` は true、他は拒否）。
pub fn parse_flag_env(raw: Option<&str>) -> Result<bool, CosineMethodError> {
    match raw {
        None | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(CosineMethodError::BadEnv(other.to_string())),
    }
}

fn l2_normalize_f64(v: &mut [f32]) {
    let n: f64 = v
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    if n > 0.0 {
        for x in v.iter_mut() {
            *x = (*x as f64 / n) as f32;
        }
    }
}

/// 決定的 fixture を生成する。
pub fn generate_fixture(
    kind: FixtureKind,
    seed: u64,
    dim: usize,
    rows: usize,
) -> Result<Fixture, CosineMethodError> {
    if dim == 0 || dim > MAX_DIM || rows == 0 || rows > MAX_ROWS {
        return Err(CosineMethodError::BadShape);
    }
    let mut rng = DeterministicRng::new(seed);
    let mut query = rng.next_vector(dim);
    let mut data = Vec::with_capacity(rows * dim);
    match kind {
        FixtureKind::Normalized => {
            l2_normalize_f64(&mut query);
            for _ in 0..rows {
                let mut v = rng.next_vector(dim);
                l2_normalize_f64(&mut v);
                data.extend_from_slice(&v);
            }
        }
        FixtureKind::Scattered | FixtureKind::WithZero => {
            for i in 0..rows {
                let mut v = rng.next_vector(dim);
                l2_normalize_f64(&mut v);
                // 指数を [-3, 3) に一様に取り、ノルム 1e-3〜1e3 に散らす。
                let exp = rng.next_f32() * 6.0 - 3.0;
                let scale = 10f32.powf(exp);
                let zero = kind == FixtureKind::WithZero && i % 100 == 7;
                for x in v.iter_mut() {
                    *x = if zero { 0.0 } else { *x * scale };
                }
                data.extend_from_slice(&v);
            }
            for x in query.iter_mut() {
                *x *= 37.5;
            }
        }
        FixtureKind::NearDuplicate => {
            // 相対摂動 eps から 1 - cos はおよそ eps^2 / 2 になるので、eps が 1.4e-3 前後で
            // 距離 1e-6 程度の行が並ぶ。摂動幅は行ごとに 0.5〜1.5 倍へ振る。
            for _ in 0..rows {
                let jitter = 0.5 + rng.next_f32();
                let eps = 1.4e-3 * jitter;
                let v: Vec<f32> = query
                    .iter()
                    .map(|q| q * (1.0 + eps * (rng.next_f32() * 2.0 - 1.0)))
                    .collect();
                data.extend_from_slice(&v);
            }
        }
    }
    Ok(Fixture {
        dim,
        rows: data,
        query,
    })
}

// ---------------------------------------------------------------- 参照実装

fn clamp_dist(dot: f64, norm_prod: f64) -> f64 {
    let s = dot / norm_prod.sqrt();
    // f64::clamp は NaN を保存する（ゼロノルムは NaN のまま）。
    let s = s.clamp(-1.0, 1.0);
    1.0 - s
}

/// f64 累積の厳密寄り参照距離（ゼロノルムは NaN）。
pub fn exact_cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    let (mut d, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (*x as f64, *y as f64);
        d += x * y;
        na += x * x;
        nb += y * y;
    }
    clamp_dist(d, na * nb)
}

/// pgvector 式のエミュレーション: f32 逐次累積 + f64 除算 + クランプ。
pub fn pgvector_cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    let (mut d, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        d += x * y;
        na += x * x;
        nb += y * y;
    }
    clamp_dist(d as f64, (na as f64) * (nb as f64))
}

/// 方式 A: 正規化済み前提の `1 - dot`（f32 の逐次和）。
pub fn method_a_distance(a: &[f32], b: &[f32]) -> f64 {
    let d: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    1.0 - d as f64
}

/// 方式 B′ の書き込み時変換: f64 でノルムを求め f32 へスケーリングした複製を返す。
/// ゼロノルムは全 0 のまま返す。
pub fn normalized_copy(v: &[f32]) -> Vec<f32> {
    let mut c = v.to_vec();
    l2_normalize_f64(&mut c);
    c
}

/// 8 レーン縮約（C1/C2 共通。順序を共有することでビット一致を保つ）。
#[inline(always)]
fn reduce(l: [f32; LANES]) -> f32 {
    ((l[0] + l[4]) + (l[1] + l[5])) + ((l[2] + l[6]) + (l[3] + l[7]))
}

/// 8 レーンの内積（C2 用。C1 の dot アキュムレータと同一構造）。
#[inline(never)]
pub fn dot_lanes(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; LANES];
    let (ca, ra) = a.as_chunks::<LANES>();
    let (cb, rb) = b.as_chunks::<LANES>();
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..LANES {
            acc[i] += x[i] * y[i];
        }
    }
    let mut t = 0f32;
    for (x, y) in ra.iter().zip(rb) {
        t += x * y;
    }
    reduce(acc) + t
}

/// 8 レーンの `‖x‖²`（C2 のキャッシュ値。C1 の `‖row‖²` と同一累積順）。
#[inline(never)]
pub fn norm_sq_lanes(a: &[f32]) -> f32 {
    let mut acc = [0f32; LANES];
    let (ca, ra) = a.as_chunks::<LANES>();
    for x in ca {
        for i in 0..LANES {
            acc[i] += x[i] * x[i];
        }
    }
    let mut t = 0f32;
    for x in ra {
        t += x * x;
    }
    reduce(acc) + t
}

/// 方式 C1: dot・`‖row‖²`・`‖q‖²` を 1 パスで融合計算する
/// （クエリのノルムも毎行再計算する素直な形）。
#[inline(never)]
pub fn c1_fused_distance(row: &[f32], q: &[f32]) -> f64 {
    let (mut ad, mut ar, mut aq) = ([0f32; LANES], [0f32; LANES], [0f32; LANES]);
    let (cr, rr) = row.as_chunks::<LANES>();
    let (cq, rq) = q.as_chunks::<LANES>();
    for (x, y) in cr.iter().zip(cq) {
        for i in 0..LANES {
            ad[i] += x[i] * y[i];
            ar[i] += x[i] * x[i];
            aq[i] += y[i] * y[i];
        }
    }
    let (mut td, mut tr, mut tq) = (0f32, 0f32, 0f32);
    for (x, y) in rr.iter().zip(rq) {
        td += x * y;
        tr += x * x;
        tq += y * y;
    }
    let (d, nr, nq) = (reduce(ad) + td, reduce(ar) + tr, reduce(aq) + tq);
    clamp_dist(d as f64, (nr as f64) * (nq as f64))
}

/// 方式 C2: 事前計算した `‖row‖²`・`‖q‖²`（クエリ側は 1 回だけ）と dot から距離を作る。
#[inline(always)]
pub fn c2_distance(dot: f32, row_norm_sq: f32, query_norm_sq: f32) -> f64 {
    clamp_dist(dot as f64, (row_norm_sq as f64) * (query_norm_sq as f64))
}

// ---------------------------------------------------------------- 誤差集計

/// 1 arm の誤差集計。
#[derive(Debug, Default, Clone)]
pub struct ErrorStats {
    /// 有限同士の `|Δd|` の最大値。
    pub max_abs: f64,
    /// 有限同士の `|Δd|` の平均値。
    pub mean_abs: f64,
    /// 片方だけ NaN だった行数（ゼロノルム扱いの不一致）。
    pub nan_mismatch: usize,
    /// 比較した有限同士の行数。
    pub compared: usize,
}

/// `got` を `reference` と比較する（両方 NaN は一致、片方のみ NaN は不一致として数える）。
pub fn error_stats(got: &[f64], reference: &[f64]) -> ErrorStats {
    let mut s = ErrorStats::default();
    let mut sum = 0f64;
    for (g, r) in got.iter().zip(reference) {
        match (g.is_nan(), r.is_nan()) {
            (true, true) => {}
            (false, false) => {
                let e = (g - r).abs();
                s.max_abs = s.max_abs.max(e);
                sum += e;
                s.compared += 1;
            }
            _ => s.nan_mismatch += 1,
        }
    }
    if s.compared > 0 {
        s.mean_abs = sum / s.compared as f64;
    }
    s
}

/// 距離昇順 Top-k の行 index（NaN は末尾・同値は index 昇順）。
pub fn top_k(dists: &[f64], k: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..dists.len()).collect();
    idx.sort_by(|&a, &b| {
        let (x, y) = (dists[a], dists[b]);
        match (x.is_nan(), y.is_nan()) {
            (true, true) => a.cmp(&b),
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => x.total_cmp(&y).then(a.cmp(&b)),
        }
    });
    idx.truncate(k);
    idx
}

/// Top-k の集合一致率と順序一致の有無。
pub fn topk_agreement(got: &[usize], reference: &[usize]) -> (f64, bool) {
    if reference.is_empty() {
        return (1.0, got.is_empty());
    }
    let hit = got.iter().filter(|g| reference.contains(g)).count();
    (hit as f64 / reference.len() as f64, got == reference)
}

/// C1 と C2 が全行でビット一致するか確認する（不一致の最初の行を返す）。
pub fn check_c1_c2_bit_identical(f: &Fixture) -> Result<(), usize> {
    let qn = norm_sq_lanes(&f.query);
    for (i, row) in f.iter_rows().enumerate() {
        let c1 = c1_fused_distance(row, &f.query);
        let c2 = c2_distance(dot_lanes(row, &f.query), norm_sq_lanes(row), qn);
        if c1.to_bits() != c2.to_bits() && !(c1.is_nan() && c2.is_nan()) {
            return Err(i);
        }
    }
    Ok(())
}

/// pgvector リテラル（`[1,2,3]`）へ整形する。
pub fn vector_literal(v: &[f32]) -> String {
    let body: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
    format!("[{}]", body.join(","))
}
