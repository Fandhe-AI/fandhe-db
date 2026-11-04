//! x86_64 AVX2+FMA／AVX-512F 向け 4 行ブロック内積カーネル本体（Issue #510・
//! TASK-156・CORE-14。ポインタ: `docs/design/dot-kernel-row-block.md`）。
//!
//! 呼び出し文脈: 親モジュール [`super`]（`isa.rs`）の
//! `SimdKernel::dot_block4_impl` が、sealed トークン（[`super::Avx2FmaToken`]／
//! [`super::Avx512Token`]）の所持を SAFETY 根拠とする `unsafe` 呼び出し 2 箇所
//! （AVX2+FMA・AVX-512）からのみ本モジュールの関数を呼ぶ。カーネル本体自体は
//! ポインタ load/store・`transmute`・raw pointer キャストを一切使わない safe fn
//! とし、`unsafe` は `isa.rs` のトークンディスパッチ箇所以外に持ち込まない
//! （ADR `docs/design/simd-intrinsics-adoption.md` 決定 1）。
//!
//! # ビット同一性の根拠
//!
//! 1 行版 [`super::dot_lanes`] とスコアがビット同一であることは次の 3 点で
//! 構造的に担保する:
//! 1. `_mm256_fmadd_ps`／`_mm512_fmadd_ps` はレーンごとの単一丸め FMA であり、
//!    `f32::mul_add`（[`super::dot_lanes`] が使う演算）と同一の丸め結果になる。
//! 2. `as_chunks` によるチャンク走査順・レーン位置（`load8`／`load16` が
//!    構築する SIMD レジスタの各レーン、[`lane_sum8`]／[`lane_sum16`] が取り出す
//!    順序）が 1 行版と同一になるよう `_mm256_set_ps`／`_mm512_set_ps` へ引数を
//!    逆順（レーン 7→0／15→0）で渡し、レーン和はインデックス昇順の
//!    `0.0 + l0 + l1 + ... `（`[f32; LANES]::iter().sum()` と同じ左畳み込み）で
//!    計算する。
//! 3. 端数和は 1 行版と完全に同じ関数 [`super::tail_sum`] を呼ぶ。
//!
//! `crates/engine/tests/isa.rs` の `dot_block4_matches_single_row_dot_bit_exact_*`
//! がこれを機械検証する。
//!
//! # レーン和をスカラー直接縮約にした理由（生成コード検査対応）
//!
//! 当初は [`super::reduce_lanes`] へ `[f32; LANES]` を渡す構成にしていたが、
//! `scripts/check_simd_codegen.sh` の実測で、抽出した 4〜16 個のスカラーを
//! いったん `[f32; LANES]` へ詰めてから `iter().sum()` する形は、SLP
//! ベクトライザがこの詰め直しを `vinsertps`／`vunpcklps`／`vunpckhps`
//! （要素ごと挿入・組み立て命令。ADR 決定 2 で禁止する命令）へ再パックしてしまう
//! ことが判明した（詳細・実測ログは `docs/design/dot-kernel-row-block.md` 参照）。
//! [`lane_sum8`]／[`lane_sum16`] は配列を経由せず、SIMD レジスタから取り出した
//! スカラーへ直接 `+=` の逐次和を適用することでこの再パックを回避する。

use std::arch::x86_64::*;

/// `c` の 8 要素を `__m256` へロードする（レーン 0..7 が `c[0..8]` と対応）。
///
/// ポインタ load（`_mm256_loadu_ps`）を使わず、配列をパターン分解して
/// `_mm256_set_ps`（引数はレーン 7→0 の逆順）へ渡す。`as_chunks::<8>()` で得た
/// `&[f32; 8]` を経由するため常に境界内アクセスで、添字（`[]`）は使わない
/// （.claude/rules/coding-rust.md）。生成コードは `scripts/check_simd_codegen.sh`
/// が実測検査する（要素ごと挿入命令 0 件・単一の `vmovups` へ畳み込まれることを
/// `docs/design/simd-codegen-guard.md` に記録）。
#[target_feature(enable = "avx2,fma")]
#[inline]
fn load8(c: &[f32; 8]) -> __m256 {
    let [c0, c1, c2, c3, c4, c5, c6, c7] = *c;
    _mm256_set_ps(c7, c6, c5, c4, c3, c2, c1, c0)
}

/// `__m256` の 8 レーンをインデックス昇順に取り出して和を返す（モジュール doc
/// 「レーン和をスカラー直接縮約にした理由」参照。`[f32; 8]` を経由しないため
/// SLP による要素ごと挿入命令の再混入が起きない）。`_mm256_castps256_ps128`／
/// `_mm256_extractf128_ps` で上下 128 bit（`__m128`）へ分解したあと、
/// `_mm_permute_ps` でレーンを先頭へ回してから `_mm_cvtss_f32` で取り出す。
/// 和の計算順は `[f32; 8]::iter().sum()`（加算の単位元 `-0.0` を初期値とする
/// `fold(-0.0, Add::add)`）と同一の
/// `(((((((-0.0 + l0) + l1) + l2) + l3) + l4) + l5) + l6) + l7` にし、
/// [`super::lane_sum`] とビット同一になるようにする（符号付きゼロを含む境界値でも
/// 一致させるため、初期値は `+0.0` ではなく `-0.0` を用いる）。
#[target_feature(enable = "avx2,fma")]
#[inline]
fn lane_sum8(v: __m256) -> f32 {
    let lo = _mm256_castps256_ps128(v);
    let hi = _mm256_extractf128_ps::<1>(v);
    let l0 = _mm_cvtss_f32(lo);
    let l1 = _mm_cvtss_f32(_mm_permute_ps::<0b01_01_01_01>(lo));
    let l2 = _mm_cvtss_f32(_mm_permute_ps::<0b10_10_10_10>(lo));
    let l3 = _mm_cvtss_f32(_mm_permute_ps::<0b11_11_11_11>(lo));
    let l4 = _mm_cvtss_f32(hi);
    let l5 = _mm_cvtss_f32(_mm_permute_ps::<0b01_01_01_01>(hi));
    let l6 = _mm_cvtss_f32(_mm_permute_ps::<0b10_10_10_10>(hi));
    let l7 = _mm_cvtss_f32(_mm_permute_ps::<0b11_11_11_11>(hi));

    let mut sum = -0.0f32;
    sum += l0;
    sum += l1;
    sum += l2;
    sum += l3;
    sum += l4;
    sum += l5;
    sum += l6;
    sum += l7;
    sum
}

/// AVX2+FMA（256 bit・8 レーン）の 4 行ブロック内積本体。
///
/// 呼び出し元（`isa.rs::SimdKernel::dot_block4_impl`）が 4 行と `query` の長さが
/// すべて等しいことを保証済みの前提で呼ぶ（本関数自体は長さ検証を行わない）。
/// クエリチャンクは 1 回だけロードして 4 本のアキュムレータへ再利用し、行側の
/// ロードは `_mm256_fmadd_ps` の第 1 引数として都度構築する
/// （メモリオペランド畳み込みは LLVM の最適化に委ねる。生成コードは
/// `docs/design/simd-codegen-guard.md`「§5 基線」参照）。
///
/// 4 件の結果は `[f32; 4]` の戻り値ではなく 4 個の独立した `&mut f32` 出力引数
/// （`out0..out3`）へ書き込む（モジュール doc「レーン和をスカラー直接縮約に
/// した理由」の続き: `[f32; 4]` を 1 回の戻り値として返すと、`#[target_feature]`
/// が有効な本関数の中では SLP ベクトライザが 4 本の水平和をまとめて
/// `vinsertps`／`vunpcklps` で 1 個のベクタへ再構成してから 1 回の `vmovups` で
/// 返す最適化を行ってしまうことが実測で判明した。呼び出し元
/// `isa.rs::SimdKernel::dot_block4_impl` は `#[target_feature]` を持たない
/// プレーンな関数であり、そちらで `[s0, s1, s2, s3]` を組み立てれば AVX
/// ベクタ化の対象にならず、この再パックが再混入しない。詳細・実測ログは
/// `docs/design/dot-kernel-row-block.md` 参照）。
#[target_feature(enable = "avx2,fma")]
pub(super) fn dot_block4_avx2_fma<const PADDED_TAIL: bool>(
    rows: [&[f32]; 4],
    query: &[f32],
    out0: &mut f32,
    out1: &mut f32,
    out2: &mut f32,
    out3: &mut f32,
) {
    let [r0, r1, r2, r3] = rows;
    let (qc, qr) = query.as_chunks::<8>();
    let (c0, t0) = r0.as_chunks::<8>();
    let (c1, t1) = r1.as_chunks::<8>();
    let (c2, t2) = r2.as_chunks::<8>();
    let (c3, t3) = r3.as_chunks::<8>();

    let mut a0 = _mm256_setzero_ps();
    let mut a1 = _mm256_setzero_ps();
    let mut a2 = _mm256_setzero_ps();
    let mut a3 = _mm256_setzero_ps();

    for ((((qk, x0), x1), x2), x3) in qc.iter().zip(c0).zip(c1).zip(c2).zip(c3) {
        let vq = load8(qk);
        a0 = _mm256_fmadd_ps(load8(x0), vq, a0);
        a1 = _mm256_fmadd_ps(load8(x1), vq, a1);
        a2 = _mm256_fmadd_ps(load8(x2), vq, a2);
        a3 = _mm256_fmadd_ps(load8(x3), vq, a3);
    }

    *out0 = lane_sum8(a0) + super::tail_sum::<8, PADDED_TAIL>(t0, qr);
    *out1 = lane_sum8(a1) + super::tail_sum::<8, PADDED_TAIL>(t1, qr);
    *out2 = lane_sum8(a2) + super::tail_sum::<8, PADDED_TAIL>(t2, qr);
    *out3 = lane_sum8(a3) + super::tail_sum::<8, PADDED_TAIL>(t3, qr);
}

/// `c` の 16 要素を `__m512` へロードする（[`load8`] と同じ方針。レーン 0..15 が
/// `c[0..16]` と対応。`_mm512_set_ps` は引数をレーン 15→0 の逆順で渡す）。
#[target_feature(enable = "avx512f")]
#[inline]
fn load16(c: &[f32; 16]) -> __m512 {
    let [c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14, c15] = *c;
    _mm512_set_ps(
        c15, c14, c13, c12, c11, c10, c9, c8, c7, c6, c5, c4, c3, c2, c1, c0,
    )
}

/// `__m512` の 16 レーンをインデックス昇順に取り出して和を返す（[`lane_sum8`] の
/// 512 bit 版）。`_mm512_castps512_ps128`／`_mm512_extractf32x4_ps` で 4 つの
/// `__m128` へ分解してから [`lane_sum8`] と同じ `_mm_permute_ps`＋`_mm_cvtss_f32`
/// で取り出し、加算の単位元 `-0.0` から始まる左畳み込みで和を計算する
/// （[`lane_sum8`] と同じ理由で `+0.0` ではなく `-0.0` を初期値にする）。
#[target_feature(enable = "avx512f")]
#[inline]
fn lane_sum16(v: __m512) -> f32 {
    let q0 = _mm512_castps512_ps128(v);
    let q1 = _mm512_extractf32x4_ps::<1>(v);
    let q2 = _mm512_extractf32x4_ps::<2>(v);
    let q3 = _mm512_extractf32x4_ps::<3>(v);

    let l0 = _mm_cvtss_f32(q0);
    let l1 = _mm_cvtss_f32(_mm_permute_ps::<0b01_01_01_01>(q0));
    let l2 = _mm_cvtss_f32(_mm_permute_ps::<0b10_10_10_10>(q0));
    let l3 = _mm_cvtss_f32(_mm_permute_ps::<0b11_11_11_11>(q0));
    let l4 = _mm_cvtss_f32(q1);
    let l5 = _mm_cvtss_f32(_mm_permute_ps::<0b01_01_01_01>(q1));
    let l6 = _mm_cvtss_f32(_mm_permute_ps::<0b10_10_10_10>(q1));
    let l7 = _mm_cvtss_f32(_mm_permute_ps::<0b11_11_11_11>(q1));
    let l8 = _mm_cvtss_f32(q2);
    let l9 = _mm_cvtss_f32(_mm_permute_ps::<0b01_01_01_01>(q2));
    let l10 = _mm_cvtss_f32(_mm_permute_ps::<0b10_10_10_10>(q2));
    let l11 = _mm_cvtss_f32(_mm_permute_ps::<0b11_11_11_11>(q2));
    let l12 = _mm_cvtss_f32(q3);
    let l13 = _mm_cvtss_f32(_mm_permute_ps::<0b01_01_01_01>(q3));
    let l14 = _mm_cvtss_f32(_mm_permute_ps::<0b10_10_10_10>(q3));
    let l15 = _mm_cvtss_f32(_mm_permute_ps::<0b11_11_11_11>(q3));

    let mut sum = -0.0f32;
    sum += l0;
    sum += l1;
    sum += l2;
    sum += l3;
    sum += l4;
    sum += l5;
    sum += l6;
    sum += l7;
    sum += l8;
    sum += l9;
    sum += l10;
    sum += l11;
    sum += l12;
    sum += l13;
    sum += l14;
    sum += l15;
    sum
}

/// AVX-512F（512 bit・16 レーン）の 4 行ブロック内積本体。構造は
/// [`dot_block4_avx2_fma`] と同一（`avx512f` は FMA を含意する）。
///
/// 実行時検証は本開発環境（AVX-512F 非搭載）では行えず、コンパイル・命令検査
/// （`make simd-codegen-check`）と 1 行版とのロジック対称性のみで担保する
/// （実機検証・前後比較は Issue #512／#530 の担当。詳細は
/// `docs/design/dot-kernel-row-block.md` 参照）。
/// 4 件の結果を `&mut f32` 出力引数へ書き込む理由は [`dot_block4_avx2_fma`] と
/// 同じ（SLP による `[f32; 4]` 再パック回避。`docs/design/dot-kernel-row-block.md`
/// 参照）。
#[target_feature(enable = "avx512f")]
pub(super) fn dot_block4_avx512<const PADDED_TAIL: bool>(
    rows: [&[f32]; 4],
    query: &[f32],
    out0: &mut f32,
    out1: &mut f32,
    out2: &mut f32,
    out3: &mut f32,
) {
    let [r0, r1, r2, r3] = rows;
    let (qc, qr) = query.as_chunks::<16>();
    let (c0, t0) = r0.as_chunks::<16>();
    let (c1, t1) = r1.as_chunks::<16>();
    let (c2, t2) = r2.as_chunks::<16>();
    let (c3, t3) = r3.as_chunks::<16>();

    let mut a0 = _mm512_setzero_ps();
    let mut a1 = _mm512_setzero_ps();
    let mut a2 = _mm512_setzero_ps();
    let mut a3 = _mm512_setzero_ps();

    for ((((qk, x0), x1), x2), x3) in qc.iter().zip(c0).zip(c1).zip(c2).zip(c3) {
        let vq = load16(qk);
        a0 = _mm512_fmadd_ps(load16(x0), vq, a0);
        a1 = _mm512_fmadd_ps(load16(x1), vq, a1);
        a2 = _mm512_fmadd_ps(load16(x2), vq, a2);
        a3 = _mm512_fmadd_ps(load16(x3), vq, a3);
    }

    *out0 = lane_sum16(a0) + super::tail_sum::<16, PADDED_TAIL>(t0, qr);
    *out1 = lane_sum16(a1) + super::tail_sum::<16, PADDED_TAIL>(t1, qr);
    *out2 = lane_sum16(a2) + super::tail_sum::<16, PADDED_TAIL>(t2, qr);
    *out3 = lane_sum16(a3) + super::tail_sum::<16, PADDED_TAIL>(t3, qr);
}
// ---------------------------------------------------------------------------
// L2 二乗距離カーネル（Issue #1982・SQL-33。ポインタ: `docs/spec/04-behavior/`、
// 設計: `docs/design/dot-kernel-row-block.md`）
// ---------------------------------------------------------------------------
//
// 呼び出し元は `isa.rs::SimdKernel::l2_sq`／`l2_sq_block4` の `unsafe` ディスパッチ
// （AVX2+FMA・AVX-512 各 2 箇所）。aarch64 版 `neon_block4::l2_sq_neon` と同じ演算列
// （`d = 行 - クエリ` → `acc = fma(d, d, acc)` → 逐次レーン和 → 共有の端数和）で、
// 単一行版と 4 行版のビット同一性を構造で担保する。

/// L2 二乗距離の端数和（`as_chunks` で割り切れない末尾要素の逐次和）。
///
/// 単一行版と 4 行版が同一関数を共有し、両者のビット同一性を担保する。`d * d` は
/// FMA 化せず（Rust は暗黙に融合しない）、`-0.0` 起点の左畳み込み（`iter().sum()`）で
/// 縮約する。`isa::l2_sq_scalar` の端数と同じ演算なので、空端数でも符号が揃う。
#[inline(always)]
fn l2_tail_sum(a_rem: &[f32], b_rem: &[f32]) -> f32 {
    a_rem
        .iter()
        .zip(b_rem.iter())
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

/// AVX2+FMA（256 bit・8 レーン）の単一行 L2 二乗距離本体。
///
/// 長さが異なる場合は短い方へ切り詰める（`isa::l2_sq_scalar` と同じ意味論。
/// `min` で境界を決めるため panic しない）。
#[target_feature(enable = "avx2,fma")]
pub(super) fn l2_sq_avx2_fma(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    let (ac, at) = a.split_at(len).0.as_chunks::<8>();
    let (bc, bt) = b.split_at(len).0.as_chunks::<8>();

    let mut acc = _mm256_setzero_ps();
    for (x, q) in ac.iter().zip(bc) {
        let d = _mm256_sub_ps(load8(x), load8(q));
        acc = _mm256_fmadd_ps(d, d, acc);
    }
    lane_sum8(acc) + l2_tail_sum(at, bt)
}

/// AVX2+FMA の 4 行ブロック L2 二乗距離本体。
///
/// 呼び出し元が 4 行と `query` の長さがすべて等しいことを保証済みの前提で呼ぶ。
/// クエリチャンクは 1 回だけロードして 4 本のアキュムレータで共有し、各行の演算列は
/// [`l2_sq_avx2_fma`] と同一（ビット同一）。結果を `&mut f32` 出力引数にする理由は
/// [`dot_block4_avx2_fma`] と同じ（SLP による再パック回避）。
#[target_feature(enable = "avx2,fma")]
pub(super) fn l2_sq_block4_avx2_fma(
    rows: [&[f32]; 4],
    query: &[f32],
    out0: &mut f32,
    out1: &mut f32,
    out2: &mut f32,
    out3: &mut f32,
) {
    let [r0, r1, r2, r3] = rows;
    let (qc, qr) = query.as_chunks::<8>();
    let (c0, t0) = r0.as_chunks::<8>();
    let (c1, t1) = r1.as_chunks::<8>();
    let (c2, t2) = r2.as_chunks::<8>();
    let (c3, t3) = r3.as_chunks::<8>();

    let mut a0 = _mm256_setzero_ps();
    let mut a1 = _mm256_setzero_ps();
    let mut a2 = _mm256_setzero_ps();
    let mut a3 = _mm256_setzero_ps();

    for ((((qk, x0), x1), x2), x3) in qc.iter().zip(c0).zip(c1).zip(c2).zip(c3) {
        let vq = load8(qk);
        let d0 = _mm256_sub_ps(load8(x0), vq);
        let d1 = _mm256_sub_ps(load8(x1), vq);
        let d2 = _mm256_sub_ps(load8(x2), vq);
        let d3 = _mm256_sub_ps(load8(x3), vq);
        a0 = _mm256_fmadd_ps(d0, d0, a0);
        a1 = _mm256_fmadd_ps(d1, d1, a1);
        a2 = _mm256_fmadd_ps(d2, d2, a2);
        a3 = _mm256_fmadd_ps(d3, d3, a3);
    }

    *out0 = lane_sum8(a0) + l2_tail_sum(t0, qr);
    *out1 = lane_sum8(a1) + l2_tail_sum(t1, qr);
    *out2 = lane_sum8(a2) + l2_tail_sum(t2, qr);
    *out3 = lane_sum8(a3) + l2_tail_sum(t3, qr);
}

/// AVX-512F（512 bit・16 レーン）の単一行 L2 二乗距離本体。構造は
/// [`l2_sq_avx2_fma`] と同一。実行時検証は AVX-512F 非搭載環境では行えず、
/// コンパイル・命令検査と AVX2 版との構造対称性で担保する。
#[target_feature(enable = "avx512f")]
pub(super) fn l2_sq_avx512(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    let (ac, at) = a.split_at(len).0.as_chunks::<16>();
    let (bc, bt) = b.split_at(len).0.as_chunks::<16>();

    let mut acc = _mm512_setzero_ps();
    for (x, q) in ac.iter().zip(bc) {
        let d = _mm512_sub_ps(load16(x), load16(q));
        acc = _mm512_fmadd_ps(d, d, acc);
    }
    lane_sum16(acc) + l2_tail_sum(at, bt)
}

/// AVX-512F の 4 行ブロック L2 二乗距離本体。構造と前提は
/// [`l2_sq_block4_avx2_fma`] と同一。
#[target_feature(enable = "avx512f")]
pub(super) fn l2_sq_block4_avx512(
    rows: [&[f32]; 4],
    query: &[f32],
    out0: &mut f32,
    out1: &mut f32,
    out2: &mut f32,
    out3: &mut f32,
) {
    let [r0, r1, r2, r3] = rows;
    let (qc, qr) = query.as_chunks::<16>();
    let (c0, t0) = r0.as_chunks::<16>();
    let (c1, t1) = r1.as_chunks::<16>();
    let (c2, t2) = r2.as_chunks::<16>();
    let (c3, t3) = r3.as_chunks::<16>();

    let mut a0 = _mm512_setzero_ps();
    let mut a1 = _mm512_setzero_ps();
    let mut a2 = _mm512_setzero_ps();
    let mut a3 = _mm512_setzero_ps();

    for ((((qk, x0), x1), x2), x3) in qc.iter().zip(c0).zip(c1).zip(c2).zip(c3) {
        let vq = load16(qk);
        let d0 = _mm512_sub_ps(load16(x0), vq);
        let d1 = _mm512_sub_ps(load16(x1), vq);
        let d2 = _mm512_sub_ps(load16(x2), vq);
        let d3 = _mm512_sub_ps(load16(x3), vq);
        a0 = _mm512_fmadd_ps(d0, d0, a0);
        a1 = _mm512_fmadd_ps(d1, d1, a1);
        a2 = _mm512_fmadd_ps(d2, d2, a2);
        a3 = _mm512_fmadd_ps(d3, d3, a3);
    }

    *out0 = lane_sum16(a0) + l2_tail_sum(t0, qr);
    *out1 = lane_sum16(a1) + l2_tail_sum(t1, qr);
    *out2 = lane_sum16(a2) + l2_tail_sum(t2, qr);
    *out3 = lane_sum16(a3) + l2_tail_sum(t3, qr);
}
