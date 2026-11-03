//! aarch64 NEON（Apple M／Graviton）向け 4 行ブロック内積・L2 二乗距離カーネル本体
//! （内積: Issue #511・TASK-156・CORE-14。L2: Issue #1983・SQL-33 のポインタ。
//! ポインタ: `docs/design/dot-kernel-row-block.md`）。
//!
//! 呼び出し文脈: 親モジュール [`super`]（`isa.rs`）の
//! `SimdKernel::dot_block4_impl` が、sealed トークン（[`super::NeonToken`]）の
//! 所持を SAFETY 根拠とする `unsafe` 呼び出し 1 箇所（Neon）からのみ本モジュールの
//! 関数を呼ぶ。カーネル本体自体はポインタ load/store・`transmute`・raw pointer
//! キャストを一切使わない safe fn とし、`unsafe` は `isa.rs` のトークン
//! ディスパッチ箇所以外に持ち込まない（ADR `docs/design/simd-intrinsics-adoption.md`
//! 決定 1。x86_64 側 [`super::x86_block4`] と同じ方針）。
//!
//! # ビット同一性の根拠
//!
//! 1 行版 [`super::dot_lanes`]（`LANES = 4`）とスコアがビット同一であることは
//! 次の 3 点で構造的に担保する:
//! 1. `vfmaq_f32` はレーンごとの単一丸め FMA であり、`f32::mul_add`
//!    （[`super::dot_lanes`] が使う演算）と同一の丸め結果になる。
//! 2. `as_chunks` によるチャンク走査順・レーン位置（[`load4`] が構築する
//!    128 bit レジスタの各レーン、[`lane_sum4`] が取り出す順序）が 1 行版と
//!    同一になるよう、`vsetq_lane_f32::<0..3>` を昇順（レーン 0→3）で適用する
//!    （x86 版が `_mm256_set_ps` へ引数を逆順で渡すのと違い、NEON の
//!    `vsetq_lane_f32` はレーン番号を明示指定するため逆順にする必要がない）。
//!    レーン和はインデックス昇順の `-0.0 + l0 + l1 + l2 + l3`
//!    （`[f32; 4]::iter().sum()` と同じ左畳み込み）で計算する。
//! 3. 端数和は 1 行版と完全に同じ関数 [`super::tail_sum`] を呼ぶ。
//!
//! `crates/engine/tests/isa.rs` の `dot_block4_matches_single_row_dot_bit_exact_*`
//! がこれを機械検証する（aarch64 実機は `detect-apple` ジョブが唯一の実行時証跡）。
//!
//! # L2 二乗距離カーネル（Issue #1983・SQL-33）
//!
//! [`l2_sq_neon`]（単一行）と [`l2_sq_block4_neon`]（4 行）は、いずれも
//! `d = vsubq_f32(行, クエリ)` → `acc = vfmaq_f32(acc, d, d)` → [`lane_sum4`] →
//! 共通の端数和 [`l2_tail_sum`] という同一の演算列を持つ。このため
//! `SimdKernel::l2_sq_block4` と `SimdKernel::l2_sq` は全入力でビット同一になる
//! （呼び出し元は `isa.rs`）。scalar 参照 `isa::l2_sq_scalar` との差は、FMA の単一丸め
//! （scalar は `d * d` と加算の二重丸め）と加算順序のみで、相対許容差内に収まる。
//! 各項の二乗和が 2^24 未満の整数値ベクトルでは丸めが生じず scalar とビット一致する。
//!
//! # レーン和を `vaddvq_f32` にしなかった理由
//!
//! `vaddvq_f32`（ペアワイズ水平加算）は `[f32; 4]::iter().sum()`
//! （左から右への逐次和）と加算順序が異なり、丸め誤差がビット不一致になり得る
//! （[`super::dot_f16_neon_fp16`] が `vaddvq_f32` を使えているのは、あちらが
//! 許容差内一致のみを契約とし ISA 間ビット同一性を要求しないため。本カーネルは
//! 1 行版とのビット同一性が契約のため踏襲できない）。[`lane_sum4`] は
//! `vgetq_lane_f32` でレーンを 1 つずつ取り出し、[`x86_block4::lane_sum8`] と
//! 同じ逐次 `+=`（初期値 `-0.0`。加算の単位元に合わせた意図的な値。符号付き
//! ゼロ境界値の一致のため）で縮約する。
//!
//! # `[f32; 4]` 戻り値ではなく `&mut f32` 出力引数にした理由
//!
//! x86 版 [`super::x86_block4::dot_block4_avx2_fma`] と同じ理由（doc 参照）。
//! 事前検証（`--target aarch64-unknown-linux-gnu -O --emit asm` での試作
//! コンパイル）で、`[f32; 4]` を 1 回の戻り値として返す形は 4 本の水平和を
//! `zip1`/`zip2`/`mov v.s[i]`（要素ごと再パック命令。ADR 決定 2 で禁止する
//! 命令）へ再構成することが判明した。本関数（`#[target_feature]` 付き）は
//! 4 個の独立した `&mut f32` へ書き込み、呼び出し元
//! `isa.rs::SimdKernel::dot_block4_impl`（`#[target_feature]` を持たない
//! プレーンな関数）側で `[s0, s1, s2, s3]` を組み立てることで、この再パックを
//! 回避する。

use std::arch::aarch64::*;

/// `c` の 4 要素を `float32x4_t` へロードする（レーン 0..3 が `c[0..4]` と対応）。
///
/// ポインタ load（`vld1q_f32`）を使わず、配列をパターン分解して
/// `vsetq_lane_f32::<0..3>` を昇順に適用する。`as_chunks::<4>()` で得た
/// `&[f32; 4]` を経由するため常に境界内アクセスで、添字（`[]`）は使わない
/// （.claude/rules/coding-rust.md）。生成コードは `scripts/check_simd_codegen.sh`
/// が実測検査する（レーン指定命令〔`ins v`／`mov v.s[i]`／`ld1 {}[n]`〕0 件・
/// 単一の `ldr q` へ畳み込まれることを `docs/design/dot-kernel-row-block.md` に
/// 記録）。
#[target_feature(enable = "neon")]
#[inline]
fn load4(c: &[f32; 4]) -> float32x4_t {
    let [c0, c1, c2, c3] = *c;
    let v = vdupq_n_f32(0.0);
    let v = vsetq_lane_f32::<0>(c0, v);
    let v = vsetq_lane_f32::<1>(c1, v);
    let v = vsetq_lane_f32::<2>(c2, v);
    vsetq_lane_f32::<3>(c3, v)
}

/// `float32x4_t` の 4 レーンをインデックス昇順に取り出して和を返す（モジュール
/// doc「レーン和を `vaddvq_f32` にしなかった理由」参照）。和の計算順は
/// `[f32; 4]::iter().sum()`（加算の単位元 `-0.0` を初期値とする
/// `fold(-0.0, Add::add)`）と同一の `((( -0.0 + l0) + l1) + l2) + l3` にし、
/// [`super::lane_sum`] とビット同一になるようにする（符号付きゼロを含む境界値
/// でも一致させるため、初期値は `+0.0` ではなく `-0.0` を用いる）。
#[target_feature(enable = "neon")]
#[inline]
fn lane_sum4(v: float32x4_t) -> f32 {
    let l0 = vgetq_lane_f32::<0>(v);
    let l1 = vgetq_lane_f32::<1>(v);
    let l2 = vgetq_lane_f32::<2>(v);
    let l3 = vgetq_lane_f32::<3>(v);

    let mut sum = -0.0f32;
    sum += l0;
    sum += l1;
    sum += l2;
    sum += l3;
    sum
}

/// NEON（128 bit・4 レーン）の 4 行ブロック内積本体。
///
/// 呼び出し元（`isa.rs::SimdKernel::dot_block4_impl`）が 4 行と `query` の長さが
/// すべて等しいことを保証済みの前提で呼ぶ（本関数自体は長さ検証を行わない）。
/// クエリチャンクは 1 回だけロードして 4 本のアキュムレータへ再利用し、行側の
/// ロードは `vfmaq_f32` の第 2 引数として都度構築する。
///
/// 4 件の結果は `[f32; 4]` の戻り値ではなく 4 個の独立した `&mut f32` 出力引数
/// （`out0..out3`）へ書き込む（モジュール doc「`[f32; 4]` 戻り値ではなく
/// `&mut f32` 出力引数にした理由」参照）。
///
/// `#[inline(never)]` は必須: NEON は aarch64 の baseline のため付けないと
/// 呼び出し元 `isa.rs::SimdKernel::dot_block4_impl` へインライン化され、
/// `check_simd_codegen.sh` の必須シンボル検査が対象を独立関数として見つけられなく
/// なる（[`super::dot_f16_neon_fp16`] と同じ理由。ADR 決定 2）。
#[target_feature(enable = "neon")]
#[inline(never)]
pub(super) fn dot_block4_neon<const PADDED_TAIL: bool>(
    rows: [&[f32]; 4],
    query: &[f32],
    out0: &mut f32,
    out1: &mut f32,
    out2: &mut f32,
    out3: &mut f32,
) {
    let [r0, r1, r2, r3] = rows;
    let (qc, qr) = query.as_chunks::<4>();
    let (c0, t0) = r0.as_chunks::<4>();
    let (c1, t1) = r1.as_chunks::<4>();
    let (c2, t2) = r2.as_chunks::<4>();
    let (c3, t3) = r3.as_chunks::<4>();

    let mut a0 = vdupq_n_f32(0.0);
    let mut a1 = vdupq_n_f32(0.0);
    let mut a2 = vdupq_n_f32(0.0);
    let mut a3 = vdupq_n_f32(0.0);

    for ((((qk, x0), x1), x2), x3) in qc.iter().zip(c0).zip(c1).zip(c2).zip(c3) {
        let vq = load4(qk);
        a0 = vfmaq_f32(a0, load4(x0), vq);
        a1 = vfmaq_f32(a1, load4(x1), vq);
        a2 = vfmaq_f32(a2, load4(x2), vq);
        a3 = vfmaq_f32(a3, load4(x3), vq);
    }

    *out0 = lane_sum4(a0) + super::tail_sum::<4, PADDED_TAIL>(t0, qr);
    *out1 = lane_sum4(a1) + super::tail_sum::<4, PADDED_TAIL>(t1, qr);
    *out2 = lane_sum4(a2) + super::tail_sum::<4, PADDED_TAIL>(t2, qr);
    *out3 = lane_sum4(a3) + super::tail_sum::<4, PADDED_TAIL>(t3, qr);
}

/// L2 二乗距離の端数和（`as_chunks::<4>` で割り切れない末尾要素の逐次和）。
///
/// 単一行版 [`l2_sq_neon`] と 4 行版 [`l2_sq_block4_neon`] が同一関数を共有し、
/// 両者のビット同一性を構造で担保する。`d * d` は FMA 化せず（Rust は暗黙に融合しない）、
/// 加算の単位元 `-0.0` 起点の左畳み込み（`iter().sum()`）で縮約する。
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

/// NEON（128 bit・4 レーン）の単一行 L2 二乗距離本体（Issue #1983・SQL-33）。
///
/// 呼び出し元は `isa.rs::SimdKernel::l2_sq`（Neon 分岐の `unsafe` 1 箇所）。長さが
/// 異なる場合は短い方へ切り詰める（`isa::l2_sq_scalar` と同じ意味論。panic しない）。
/// `#[inline(never)]` は [`dot_block4_neon`] と同じ理由（baseline NEON のため付けないと
/// インライン化され、`check_simd_codegen.sh` の必須シンボル検査が対象を見失う）。
#[target_feature(enable = "neon")]
#[inline(never)]
pub(super) fn l2_sq_neon(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    let (ac, at) = a.split_at(len).0.as_chunks::<4>();
    let (bc, bt) = b.split_at(len).0.as_chunks::<4>();

    let mut acc = vdupq_n_f32(0.0);
    for (x, q) in ac.iter().zip(bc) {
        let d = vsubq_f32(load4(x), load4(q));
        acc = vfmaq_f32(acc, d, d);
    }
    lane_sum4(acc) + l2_tail_sum(at, bt)
}

/// NEON の 4 行ブロック L2 二乗距離本体（Issue #1983・SQL-33）。
///
/// 呼び出し元（`isa.rs::SimdKernel::l2_sq_block4`）が 4 行と `query` の長さが
/// すべて等しいことを保証済みの前提で呼ぶ。クエリチャンクは 1 回だけロードして
/// 4 本のアキュムレータで共有し、各行の演算列は [`l2_sq_neon`] と同一
/// （ビット同一）。結果は `&mut f32` 出力引数 4 個へ書く（理由はモジュール doc
/// 「`[f32; 4]` 戻り値ではなく `&mut f32` 出力引数にした理由」と同じ）。
#[target_feature(enable = "neon")]
#[inline(never)]
pub(super) fn l2_sq_block4_neon(
    rows: [&[f32]; 4],
    query: &[f32],
    out0: &mut f32,
    out1: &mut f32,
    out2: &mut f32,
    out3: &mut f32,
) {
    let [r0, r1, r2, r3] = rows;
    let (qc, qr) = query.as_chunks::<4>();
    let (c0, t0) = r0.as_chunks::<4>();
    let (c1, t1) = r1.as_chunks::<4>();
    let (c2, t2) = r2.as_chunks::<4>();
    let (c3, t3) = r3.as_chunks::<4>();

    let mut a0 = vdupq_n_f32(0.0);
    let mut a1 = vdupq_n_f32(0.0);
    let mut a2 = vdupq_n_f32(0.0);
    let mut a3 = vdupq_n_f32(0.0);

    for ((((qk, x0), x1), x2), x3) in qc.iter().zip(c0).zip(c1).zip(c2).zip(c3) {
        let vq = load4(qk);
        let d0 = vsubq_f32(load4(x0), vq);
        let d1 = vsubq_f32(load4(x1), vq);
        let d2 = vsubq_f32(load4(x2), vq);
        let d3 = vsubq_f32(load4(x3), vq);
        a0 = vfmaq_f32(a0, d0, d0);
        a1 = vfmaq_f32(a1, d1, d1);
        a2 = vfmaq_f32(a2, d2, d2);
        a3 = vfmaq_f32(a3, d3, d3);
    }

    *out0 = lane_sum4(a0) + l2_tail_sum(t0, qr);
    *out1 = lane_sum4(a1) + l2_tail_sum(t1, qr);
    *out2 = lane_sum4(a2) + l2_tail_sum(t2, qr);
    *out3 = lane_sum4(a3) + l2_tail_sum(t3, qr);
}
