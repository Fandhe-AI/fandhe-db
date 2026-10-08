//! CPU 命令セットの実行時検出（TASK-156・対象ビヘイビア: CORE-14）。
//!
//! `dispatch.rs`（TASK-155・CORE-11, CORE-12）の決定表は、それまで
//! `detect_current_isa()`（コンパイル時 `cfg(target_arch)` のみに基づく保守的な
//! 下限検出。x86_64 は常に `Scalar`）を ISA 入力にしていた。本モジュールは
//! `std::arch::is_x86_feature_detected!` / `is_aarch64_feature_detected!` による
//! 実際の CPUID/HWCAP 照会へ置き換え、その検出結果を「SIMD カーネルを呼んでよい
//! 証明」として型（sealed トークン）に閉じ込める。
//!
//! # sealed トークンと `unsafe` の最小化
//!
//! [`NeonToken`]／[`Avx2FmaToken`]／[`Avx512Token`] はフィールド private・
//! `pub(crate)` の `try_new` からしか値を作れない（crate 外は当然、crate 内の
//! 他モジュールも「対応 feature を実行時確認した」という証明なしに値を持てない）。
//! [`SimdKernel::dot`] 内で SIMD カーネルを呼ぶ 3 箇所（NEON・AVX2+FMA・AVX-512）
//! だけが `unsafe` ブロックであり、トークンの所持そのものが SAFETY 根拠になる
//! （各所の `// SAFETY:` 参照）。NEON は aarch64 の baseline feature
//! （アーキテクチャ仕様上必ず対応）だが、`#[target_feature(enable = "neon")]` fn の
//! 呼び出しはコンパイラの安全性検査上 `unsafe` を要求するため（`make check-cross`
//! でのクロスコンパイル確認時に判明）、他 2 ISA と同じ `unsafe` 呼び出し形にして
//! いる。
//!
//! # CORE-12 との整合: 上書き機構の不存在
//!
//! `dispatch.rs` モジュールドキュメントの「CORE-12」節と同じ方針で、環境変数・
//! 設定ファイル・feature flag による検出結果の上書き機構は一切設けない
//! （`tests/isa.rs::isa_source_has_no_external_override_entry_points` が
//! ソース走査で不在を検査する）。未検証の ISA 指定で `unsafe` カーネルを強制
//! 起動させる攻撃面を、機構の不存在によって構造的に排除する。
//!
//! # dim 閾値ディスパッチ（Issue #365 の条件付き再訪・Issue #517/#518）
//!
//! [`dot_lanes`] の複数アキュムレータ化（ACC=4）は Issue #365
//! （`docs/design/dot-kernel-multi-accumulator.md`）の実測で、cache 常駐の
//! dim100/128 は悪化する一方 dim768/1536 は改善することが分かっている。本モジュールは
//! その知見を踏まえ、[`DOT_MULTI_ACC_MIN_DIM`]（=768）以上の入力長でのみ
//! [`dot_lanes_multi_acc`] へ分岐し、それ未満は既存 [`dot_lanes`] を**ビット同一の
//! まま**通す（`dot_neon`／`dot_avx2_fma`／`dot_avx512` の各 `#[target_feature]` fn
//! 内で分岐する。分岐そのものは新規 `unsafe` を要さない純粋な長さ判定）。
//! dim<768 の経路・[`dot_lanes`] 本体は本変更で一切変更しない。dim>=768 は演算順が
//! 変わるため丸め誤差が変化しうるが、`kernel::dot` を共有する全 provider
//! （`CpuScalarProvider`・`ParallelSearchProvider`・バッチ経路・RLS 事前フィルタ）は
//! 同一 ISA・同一 dim で同一の分岐結果・同一の演算順を共有するため、provider 間の
//! Top-k 整合という既存設計意図は保たれる。[`SimdKernel::dot_block4`]
//! （`ParallelSearchProvider`・`parallel_search.rs::search_range` が呼ぶ行ブロック
//! カーネル）の intrinsics 本体は `dot_lanes_multi_acc` へ対応させていないため、
//! [`SimdKernel::dot_block4_impl`] は dim>=[`DOT_MULTI_ACC_MIN_DIM`] を
//! 「4 行の長さが不揃いなときの縮退経路」（1 行版 `dot_impl` を 4 回呼ぶ）へ
//! 意図的に合流させ、`dot_block4` 経由の呼び出しにも多アキュムレータ化の効果を
//! 波及させつつ 1 行版とのビット同一契約を維持する（新規 intrinsics カーネルは
//! 追加しない）。tail 処理方式の切り替え
//! （`PADDED_TAIL`。Issue #528）は [`dot_lanes_multi_acc`] にも貫通させ、
//! [`reduce_lanes`] を経由することで dim>=768 でも [`SimdKernel::dot_with_scalar_tail`]／
//! [`SimdKernel::dot_with_padded_tail`] の契約を dim<768 と同一の形で維持する
//! （手書きの逐次和で tail を再実装しない）。実測に基づく閾値の確定・チップ別実測は
//! 別 Issue（#519）の担当（`docs/design/dot-kernel-multi-accumulator.md`
//! 「Issue #518 追記」節参照）。
//!
//! # 呼び出し文脈
//!
//! - `dispatch.rs::detect_current_isa()` は本モジュールの [`current`] へ委譲する
//!   （決定表の `SimdWidth` と実際に実行されるカーネルが同一の検出結果から
//!   導かれる構造にする）。
//! - `kernel.rs::dot`（`pub(crate)`。`CpuScalarProvider`・`parallel_search.rs`・
//!   `batch_search.rs`・`rls.rs` が共有する唯一の内積実装）は本モジュールの
//!   [`current`]`().dot(..)` へ委譲する。
//!
//! # tail 処理方式の切り替え（TASK-156・CORE-14。Issue #528）
//!
//! [`dot_lanes`] の端数（`as_chunks` で割り切れない末尾要素）は、現行のスカラー
//! 逐次和（`dot_with_scalar_tail` 相当）と、零埋め固定長バッファへ詰めてから
//! レーン数ぶんの積和を取る「分岐なし tail」（`dot_with_padded_tail` 相当）の
//! 2 方式を `const PADDED_TAIL: bool` ジェネリックで切り替えられる構造にしている。
//! [`SimdKernel::dot`]（既定経路）は `DEFAULT_PADDED_TAIL`（現状 `false`）に固定して
//! おり、production の挙動は本 Issue で変更しない。両方式は
//! [`SimdKernel::dot_with_scalar_tail`]／[`SimdKernel::dot_with_padded_tail`] として
//! テスト・ベンチ向けに公開する（`hybrid::sparse_refetch_observed` と同じ、生産経路と
//! 検証経路が同一実装を共有するための hook という位置付け）。採否・既定切替の判断は
//! Issue #529 が担う（詳細・機械検証・不採用形は
//! `docs/design/dot-kernel-branchless-tail.md` 参照。spec 本文は転記しない）。
//! `unsafe` ブロックの個数（3 個）は本変更でも増減しない
//! （`tests/isa.rs::unsafe_is_confined_to_isa_module_with_safety_comments`）。
//!
//! # 行ブロック（4 行）カーネル（TASK-156・CORE-14。Issue #510）
//!
//! [`SimdKernel::dot_block4`] は 4 行 × 1 クエリの内積を、行間でクエリの
//! ロードを 1 回に共有しつつ 1 行版 `dot` とビット同一に計算する（呼び出し元は
//! `parallel_search.rs::search_range`）。x86_64（AVX2+FMA・AVX-512F）の
//! intrinsics カーネル本体は新設サブモジュール [`x86_block4`] へ分離し、
//! `unsafe` は本モジュールのトークンディスパッチ箇所（2 箇所）のみに限定する
//! （ADR `docs/design/simd-intrinsics-adoption.md` 決定 1）。これにより
//! `unsafe` ブロックの総数は 3 → 5 になる（詳細・生成コード検査で判明した
//! 問題と対処は `docs/design/dot-kernel-row-block.md` 参照。spec 本文は
//! 転記しない）。aarch64（NEON）版は新設サブモジュール [`neon_block4`] へ同じ方針
//! （intrinsics カーネル本体は `unsafe` を持たない safe fn）で分離し、
//! `dot_block4_impl` の Neon 分岐（1 箇所）から `unsafe` 呼び出しする
//! （Issue #511・TASK-156・CORE-14）。x86_64 の 2 箇所と合わせて、行ブロック
//! カーネルのディスパッチによる `unsafe` は計 3 箇所になる。

use std::sync::OnceLock;

/// x86_64 行ブロック（4 行）カーネル本体（Issue #510・TASK-156・CORE-14）。
///
/// カーネル本体は `unsafe` を持たない safe fn として分離する（ADR
/// `docs/design/simd-intrinsics-adoption.md` 決定 1: `unsafe` は `isa.rs` の
/// トークンディスパッチ箇所以外に持ち込まない）。`isa.rs` 側は
/// [`SimdKernel::dot_block4_impl`] の 2 箇所（AVX2+FMA・AVX-512）でこのモジュールの
/// 関数を `unsafe` 呼び出しする。
#[cfg(target_arch = "x86_64")]
mod x86_block4;

/// aarch64 行ブロック（4 行）カーネル本体（Issue #511・TASK-156・CORE-14）。
///
/// カーネル本体は `unsafe` を持たない safe fn として分離する（[`x86_block4`] と
/// 同じ方針・同じ ADR `docs/design/simd-intrinsics-adoption.md` 決定 1）。`isa.rs`
/// 側は [`SimdKernel::dot_block4_impl`] の Neon 分岐でこのモジュールの関数を
/// `unsafe` 呼び出しする。
#[cfg(target_arch = "aarch64")]
mod neon_block4;

/// x86_64 整数 i8×i8 dot カーネル本体（Issue #522・親 #520・前提 #521。
/// ポインタ: TASK-132・TASK-156・CORE-16）。
///
/// [`x86_block4`] と同じ方針（`unsafe` を持たない safe fn のみで構成。
/// `docs/design/simd-intrinsics-adoption.md` 決定 1）で、AVX-512 VNNI・
/// AVX-VNNI（256bit）・AVX2 i16 widen フォールバックの 3 カーネルを提供する。
/// `isa.rs` 側は [`I8Kernel::dot_i8`] の 3 箇所でこのモジュールの関数を
/// `unsafe` 呼び出しする。
#[cfg(target_arch = "x86_64")]
mod x86_i8;

/// aarch64 NEON dotprod（`vdotq_s32`）向け整数 i8×i8 dot カーネル本体
/// （Issue #525・親 #520・前提 #522。ポインタ: TASK-132・TASK-156・CORE-16）。
///
/// [`x86_i8`] と同じ方針（`unsafe` を持たない safe fn のみで構成。
/// `docs/design/simd-intrinsics-adoption.md` 決定 1）で NEON dotprod カーネル
/// を提供する。`isa.rs` 側は [`I8Kernel::dot_i8`] の `NeonDotprod` 分岐 1 箇所
/// でこのモジュールの関数を `unsafe` 呼び出しする。
#[cfg(target_arch = "aarch64")]
mod neon_i8;

/// dim 閾値ディスパッチ（Issue #517/#518）で選択する複数アキュムレータ本数。
/// Issue #365 の実測で dim768/1536 が改善した値（ACC=4）をそのまま採用する。
const DOT_ACCUMULATORS: usize = 4;

/// この長さ（`a.len().min(b.len())`）以上の入力で [`dot_lanes_multi_acc`] へ
/// 分岐する閾値（Issue #517/#518）。`pub` にして `docs/design/
/// dot-kernel-multi-accumulator.md` の前後比較・後続 Issue #519 のベンチ／テストから
/// 参照できるようにするが、外部からの上書き機構（環境変数・設定ファイル等）は
/// 一切持たない定数であり CORE-12 の禁止事項には抵触しない
/// （`tests/isa.rs::isa_source_has_no_external_override_entry_points` 参照）。
/// 閾値そのものの確定・チップ別実測は Issue #519 の担当。
pub const DOT_MULTI_ACC_MIN_DIM: usize = 768;

/// 実行時に検出された ISA。
///
/// `dispatch.rs` から本モジュールへ移設した（`dispatch.rs` は
/// `pub use crate::isa::DetectedIsa;` で既存の公開パスを維持する）。
/// `#[non_exhaustive]` にはしない（`dispatch.rs::simd_width_for` の網羅 match が
/// variant 追加時にコンパイルエラーで気付ける状態を維持するため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedIsa {
    /// SIMD 拡張なし（スカラー演算のみ）。
    Scalar,
    /// Arm Neon（128 bit）。
    Neon,
    /// x86_64 AVX2 + FMA（256 bit）。
    Avx2Fma,
    /// x86_64 AVX-512（512 bit）。
    Avx512,
}

/// aarch64 Neon 対応の実行時確認済みトークン（sealed）。
///
/// aarch64 の baseline ISA は Neon を含むことがアーキテクチャ仕様上保証されている
/// ため [`Self::try_new`] は常に `Some` を返すが、他トークンと構築契約
/// （「実行時確認を経てのみ値を持てる」）を揃える。
#[cfg(target_arch = "aarch64")]
#[derive(Debug, Clone, Copy)]
pub struct NeonToken(());

#[cfg(target_arch = "aarch64")]
impl NeonToken {
    /// crate 内からのみ呼べる（公開コンストラクタを設けない。トークンを crate 外
    /// から任意構築できないようにする、CORE-12 と同じ sealed 方針）。
    pub(crate) fn try_new() -> Option<Self> {
        if std::arch::is_aarch64_feature_detected!("neon") {
            Some(NeonToken(()))
        } else {
            None
        }
    }
}

/// x86_64 AVX2+FMA 対応の実行時確認済みトークン（sealed）。
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy)]
pub struct Avx2FmaToken(());

#[cfg(target_arch = "x86_64")]
impl Avx2FmaToken {
    /// crate 内からのみ呼べる（[`NeonToken::try_new`] と同じ sealed 方針）。
    pub(crate) fn try_new() -> Option<Self> {
        if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
        {
            Some(Avx2FmaToken(()))
        } else {
            None
        }
    }
}

/// x86_64 AVX-512（`avx512f`）対応の実行時確認済みトークン（sealed）。
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy)]
pub struct Avx512Token(());

#[cfg(target_arch = "x86_64")]
impl Avx512Token {
    /// crate 内からのみ呼べる（[`NeonToken::try_new`] と同じ sealed 方針）。
    pub(crate) fn try_new() -> Option<Self> {
        if std::arch::is_x86_feature_detected!("avx512f") {
            Some(Avx512Token(()))
        } else {
            None
        }
    }
}

/// 検出済み ISA に対応する内積カーネルを保持する variant。
///
/// トークン型を経由してのみ SIMD variant を構築できるため（[`detect`]／[`current`]
/// のみが生成元）、呼び出し元は「対応 CPU で実際に検証された」カーネルしか
/// 呼び出せない。
#[derive(Debug, Clone, Copy)]
pub enum SimdKernel {
    /// SIMD 拡張なし（[`dot_scalar`] を使う）。
    Scalar,
    /// Arm Neon。
    #[cfg(target_arch = "aarch64")]
    Neon(NeonToken),
    /// x86_64 AVX2 + FMA。
    #[cfg(target_arch = "x86_64")]
    Avx2Fma(Avx2FmaToken),
    /// x86_64 AVX-512。
    #[cfg(target_arch = "x86_64")]
    Avx512(Avx512Token),
}

impl SimdKernel {
    /// [`DetectedIsa`] への純写像（`dispatch.rs::simd_width_for` と同じ「別の判断を
    /// 持たない写像」という位置付け）。
    pub fn isa(self) -> DetectedIsa {
        match self {
            SimdKernel::Scalar => DetectedIsa::Scalar,
            #[cfg(target_arch = "aarch64")]
            SimdKernel::Neon(_) => DetectedIsa::Neon,
            #[cfg(target_arch = "x86_64")]
            SimdKernel::Avx2Fma(_) => DetectedIsa::Avx2Fma,
            #[cfg(target_arch = "x86_64")]
            SimdKernel::Avx512(_) => DetectedIsa::Avx512,
        }
    }

    /// 内積計算。トークン所持を根拠に ISA 別カーネルへ分岐する。
    ///
    /// `a`／`b` の長さが異なる場合の挙動は [`dot_scalar`]（`zip` による短い方への
    /// 切り詰め）と同一に保つ。次元の一致検証は呼び出し元（`kernel.rs::
    /// CpuScalarProvider` 等）が既に行っており、本関数はそれを前提とした純算術のみを
    /// 担う。
    ///
    /// tail 方式は [`DEFAULT_PADDED_TAIL`]（現状 `false`＝現行のスカラー逐次和）に
    /// 固定する。production 経路の挙動は Issue #528 で変更しない
    /// （採否・切替は Issue #529）。
    pub fn dot(self, a: &[f32], b: &[f32]) -> f32 {
        self.dot_impl::<DEFAULT_PADDED_TAIL>(a, b)
    }

    /// 現行のスカラー逐次和 tail（`dot_lanes` の `rem_sum` を素朴な `iter().sum()` で
    /// 計算する方式）を強制した内積計算。ビット同一性テスト（`tests/isa.rs`）・
    /// Issue #529 の A/B 計測から参照する hook で、production 経路
    /// （[`Self::dot`]）は経由しない。
    pub fn dot_with_scalar_tail(self, a: &[f32], b: &[f32]) -> f32 {
        self.dot_impl::<false>(a, b)
    }

    /// 零埋め固定長バッファによる分岐なし tail（Issue #528）を強制した内積計算。
    /// [`Self::dot_with_scalar_tail`] とのビット同一性テスト・Issue #529 の A/B
    /// 計測から参照する hook で、production 経路（[`Self::dot`]）は経由しない。
    pub fn dot_with_padded_tail(self, a: &[f32], b: &[f32]) -> f32 {
        self.dot_impl::<true>(a, b)
    }

    /// [`Self::dot`]／[`Self::dot_with_scalar_tail`]／[`Self::dot_with_padded_tail`]
    /// が共有する本体。`PADDED_TAIL` は `dot_lanes` へそのまま伝播する const
    /// ジェネリックで、SIMD カーネルを呼ぶ 3 箇所の `unsafe` ブロック構造・SAFETY
    /// 根拠はトークン所持のみに依存し `PADDED_TAIL` の値には依存しない。
    fn dot_impl<const PADDED_TAIL: bool>(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            SimdKernel::Scalar => dot_scalar(a, b),
            #[cfg(target_arch = "aarch64")]
            SimdKernel::Neon(_) => {
                // SAFETY: この variant は `NeonToken::try_new` が
                // `is_aarch64_feature_detected!("neon")` を実行時確認できた場合に
                // のみ構築される sealed トークンを保持する（下記 Avx2Fma/Avx512
                // 分岐と同じ構造）。値の存在が CPU 対応の証明であり、`dot_neon` の
                // `#[target_feature]` 契約を満たす。NEON は aarch64 の baseline
                // feature（アーキテクチャ仕様上必ず対応）だが、`#[target_feature]`
                // を付けた safe fn の呼び出しはコンパイラの安全性検査上、呼び出し側
                // コンテキストがその feature を持つことを示す `unsafe` を常に要求する
                // （`make check-cross` でのクロスコンパイル確認時に判明）。
                unsafe { dot_neon::<PADDED_TAIL>(a, b) }
            }
            #[cfg(target_arch = "x86_64")]
            SimdKernel::Avx2Fma(_) => {
                // SAFETY: この variant は `Avx2FmaToken::try_new` が
                // `is_x86_feature_detected!("avx2")` かつ `("fma")` を実行時確認
                // できた場合にのみ構築される（`pub(crate)` かつフィールド private の
                // sealed トークンのため、crate 外は当然、確認を経ない限り crate 内
                // からも値を持てない）。値の存在自体が「本 CPU が avx2+fma に対応
                // している」ことの証明であり、`dot_avx2_fma` の `#[target_feature]`
                // 契約を満たす。
                unsafe { dot_avx2_fma::<PADDED_TAIL>(a, b) }
            }
            #[cfg(target_arch = "x86_64")]
            SimdKernel::Avx512(_) => {
                // SAFETY: この variant は `Avx512Token::try_new` が
                // `is_x86_feature_detected!("avx512f")` を実行時確認できた場合に
                // のみ構築される sealed トークンを保持する（上記 Avx2Fma 分岐と同じ
                // 構造）。値の存在が CPU 対応の証明であり、`dot_avx512` の
                // `#[target_feature]` 契約を満たす。
                unsafe { dot_avx512::<PADDED_TAIL>(a, b) }
            }
        }
    }

    /// 4 行 × 1 クエリの内積（`dot` の行ブロック版。Issue #510・TASK-156・CORE-14。
    /// ポインタ: `docs/design/dot-kernel-row-block.md`）。
    ///
    /// 契約: 全入力で `dot_block4(rows, query)[i].to_bits() ==
    /// self.dot(rows[i], query).to_bits()`（1 行版とビット同一）。行間で
    /// クエリのロードを 1 回に共有し行側のロードを FMA のメモリオペランドへ
    /// 畳み込む点のみが 1 行版と異なり、各行のアキュムレータ構造
    /// （レーン FMA → レーン和 → 端数和。[`reduce_lanes`] 共有）は 1 行版と
    /// 完全に同一に保つ。
    ///
    /// `parallel_search.rs::search_range` が呼ぶ本番経路（呼び出し元は 4 行分の
    /// `dim` 長一致を `vectors.get(start..end)` により保証済み）に加え、
    /// Issue #512（前後比較・採否）の計測 hook にもなる。
    pub fn dot_block4(self, rows: [&[f32]; 4], query: &[f32]) -> [f32; 4] {
        self.dot_block4_impl::<DEFAULT_PADDED_TAIL>(rows, query)
    }

    /// [`Self::dot_block4`] の本体。`PADDED_TAIL` は [`Self::dot_impl`] と同じ
    /// 位置付け（[`Self::dot_block4`] は常に [`DEFAULT_PADDED_TAIL`] を使う）。
    ///
    /// 高速経路（intrinsics ブロックカーネル）は 4 行すべてと `query` の長さが
    /// 等しい場合に限る。1 つでも異なれば `dot_impl` を 4 回呼ぶ経路へ縮退する
    /// （`zip` による最短長への暗黙の切り詰めが 1 行ごとに独立して起こる 1 行版と
    /// 異なる結果になるのを防ぐ。production では `search_range` が `dim` 長を
    /// 保証するため常に高速経路を通る）。
    fn dot_block4_impl<const PADDED_TAIL: bool>(
        self,
        rows: [&[f32]; 4],
        query: &[f32],
    ) -> [f32; 4] {
        let [r0, r1, r2, r3] = rows;
        let uniform_len = r0.len() == query.len()
            && r1.len() == query.len()
            && r2.len() == query.len()
            && r3.len() == query.len();
        // dim>=`DOT_MULTI_ACC_MIN_DIM`（768）は 1 行版 `dot_impl`（`dot_neon`／
        // `dot_avx2_fma`／`dot_avx512` 経由で `dot_lanes_multi_acc` へ分岐する。
        // Issue #518 のスコープは「1 行版 `dot` のみ」で、本関数の intrinsics
        // ブロックカーネル（`x86_block4`／`neon_block4`）は旧来どおり `dot_lanes`
        // 相当の単一アキュムレータ構成のまま据え置く（行ブロック側の ACC=4 化は
        // Issue #519 へ申し送り）。両者を同時に呼ぶ経路をそのままにすると
        // `Self::dot_block4` の doc 契約「`dot_block4(rows, query)[i].to_bits()
        // == self.dot(rows[i], query).to_bits()`」が dim>=768 で構造的に破れる
        // （`kernel.rs::dot` を共有する `CpuScalarProvider` と
        // `ParallelSearchProvider`〔`dot_block4` 経由〕の Top-k が同一クエリ内で
        // 食い違いかねない）ため、dim>=768 は下記の「1 行版を 4 回呼ぶ」縮退経路
        // （既存の非一様長フォールバックと同型）へ意図的に合流させ、契約を維持する。
        let below_multi_acc_threshold = query.len() < DOT_MULTI_ACC_MIN_DIM;

        if !uniform_len || !below_multi_acc_threshold {
            return [
                self.dot_impl::<PADDED_TAIL>(r0, query),
                self.dot_impl::<PADDED_TAIL>(r1, query),
                self.dot_impl::<PADDED_TAIL>(r2, query),
                self.dot_impl::<PADDED_TAIL>(r3, query),
            ];
        }

        match self {
            SimdKernel::Scalar => [
                dot_scalar(r0, query),
                dot_scalar(r1, query),
                dot_scalar(r2, query),
                dot_scalar(r3, query),
            ],
            #[cfg(target_arch = "aarch64")]
            SimdKernel::Neon(_) => {
                // 4 件の結果は `[f32; 4]` の戻り値ではなく `&mut f32` 出力引数 4 個
                // で受け取る（`neon_block4::dot_block4_neon` の doc コメント
                // 「`[f32; 4]` 戻り値ではなく `&mut f32` 出力引数にした理由」参照。
                // 本関数（`#[target_feature]` を持たないプレーンな関数）側で
                // `[s0, s1, s2, s3]` を組み立てることで、SLP による要素ごと挿入
                // 命令への再パックを避ける。x86_64 Avx2Fma 分岐と同じ理由）。
                let (mut s0, mut s1, mut s2, mut s3) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                // SAFETY: この variant は `NeonToken::try_new` が
                // `is_aarch64_feature_detected!("neon")` を実行時確認できた場合に
                // のみ構築される sealed トークンを保持する（[`Self::dot_impl`] の
                // Neon 分岐と同じ SAFETY 根拠。NEON は aarch64 の baseline feature
                // だが、`#[target_feature]` を付けた fn の呼び出しはコンパイラの
                // 安全性検査上 `unsafe` を常に要求する）。値の存在が CPU 対応の
                // 証明であり、`neon_block4::dot_block4_neon` の `#[target_feature]`
                // 契約を満たす。
                unsafe {
                    neon_block4::dot_block4_neon::<PADDED_TAIL>(
                        [r0, r1, r2, r3],
                        query,
                        &mut s0,
                        &mut s1,
                        &mut s2,
                        &mut s3,
                    )
                }
                [s0, s1, s2, s3]
            }
            #[cfg(target_arch = "x86_64")]
            SimdKernel::Avx2Fma(_) => {
                // 4 件の結果は `[f32; 4]` の戻り値ではなく `&mut f32` 出力引数 4 個
                // で受け取る（`x86_block4::dot_block4_avx2_fma` の doc コメント
                // 「レーン和をスカラー直接縮約にした理由」参照。本関数
                // （`#[target_feature]` を持たないプレーンな関数）側で
                // `[s0, s1, s2, s3]` を組み立てることで、SLP による要素ごと挿入
                // 命令への再パックを避ける）。
                let (mut s0, mut s1, mut s2, mut s3) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                // SAFETY: `Avx2Fma` variant は `Avx2FmaToken::try_new` が
                // `is_x86_feature_detected!("avx2")` かつ `("fma")` を実行時確認
                // できた場合にのみ構築される sealed トークンを保持する（[`Self::dot_impl`]
                // の Avx2Fma 分岐と同じ SAFETY 根拠）。値の存在自体が本 CPU が
                // avx2+fma に対応していることの証明であり、
                // `x86_block4::dot_block4_avx2_fma` の `#[target_feature]` 契約を満たす。
                unsafe {
                    x86_block4::dot_block4_avx2_fma::<PADDED_TAIL>(
                        [r0, r1, r2, r3],
                        query,
                        &mut s0,
                        &mut s1,
                        &mut s2,
                        &mut s3,
                    )
                }
                [s0, s1, s2, s3]
            }
            #[cfg(target_arch = "x86_64")]
            SimdKernel::Avx512(_) => {
                let (mut s0, mut s1, mut s2, mut s3) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                // SAFETY: `Avx512` variant は `Avx512Token::try_new` が
                // `is_x86_feature_detected!("avx512f")` を実行時確認できた場合に
                // のみ構築される sealed トークンを保持する（[`Self::dot_impl`] の
                // Avx512 分岐と同じ SAFETY 根拠）。値の存在が CPU 対応の証明であり、
                // `x86_block4::dot_block4_avx512` の `#[target_feature]` 契約を満たす。
                unsafe {
                    x86_block4::dot_block4_avx512::<PADDED_TAIL>(
                        [r0, r1, r2, r3],
                        query,
                        &mut s0,
                        &mut s1,
                        &mut s2,
                        &mut s3,
                    )
                }
                [s0, s1, s2, s3]
            }
        }
    }
}

/// [`SimdKernel::dot`] が使う既定の tail 方式（Issue #528）。
///
/// `false` ＝現行のスカラー逐次和 tail。production 経路は本 Issue で変更せず、
/// 採否・切替（`true` への反転）は Issue #529 のオーナー判断に委ねる。
/// 環境変数・設定ファイルによる上書き機構は設けない（CORE-12 と同じ方針。
/// `tests/isa.rs::isa_source_has_no_external_override_entry_points` が検査する
/// 禁止トークン集合はこの `const` にも適用される）。
const DEFAULT_PADDED_TAIL: bool = false;

/// 優先順（AVX-512 → AVX2+FMA → NEON → Scalar）で `try_new` を試し、最初に成功した
/// トークンで [`SimdKernel`] を確定させる。いずれも失敗すれば `Scalar`
/// （fail-closed: 実際より広い ISA を主張しない）。
///
/// 呼び出す毎に CPUID/HWCAP 照会を行う（プロセス内キャッシュは [`current`] が担う）。
pub fn detect() -> SimdKernel {
    #[cfg(target_arch = "x86_64")]
    {
        if let Some(token) = Avx512Token::try_new() {
            return SimdKernel::Avx512(token);
        }
        if let Some(token) = Avx2FmaToken::try_new() {
            return SimdKernel::Avx2Fma(token);
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if let Some(token) = NeonToken::try_new() {
            return SimdKernel::Neon(token);
        }
    }
    SimdKernel::Scalar
}

/// プロセス内で 1 回だけ [`detect`] を実行し、以後は同じ値を返す（CORE-14 の
/// 「起動時または初回ディスパッチ時に検出」に対応）。CPU の対応命令セットは
/// プロセス実行中に変化しないため、繰り返し照会するコストを避ける。
/// `dispatch.rs::select_execution_path`（CORE-12: 参照透過性）が前提とする
/// 「同一入力 → 同一出力」は、本関数がプロセス内で単調であることにより保たれる。
pub fn current() -> SimdKernel {
    static CURRENT: OnceLock<SimdKernel> = OnceLock::new();
    *CURRENT.get_or_init(detect)
}

/// 内積（dot product）のスカラー参照実装（左から右への逐次和）。
///
/// `kernel.rs::dot`（`CpuScalarProvider`・`parallel_search.rs::search_range`・
/// `batch_search.rs`・`rls.rs` が共有）の実体、およびテストの参照実装として公開する。
pub fn dot_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Arm Neon（128 bit・4 レーン）向け内積カーネル。
///
/// intrinsics を使わず `#[target_feature(enable = "neon")]` を付けた safe fn とし、
/// LLVM の自動ベクトル化に委ねる（本タスクの範囲は「実行時検出とトークンによる
/// 安全な呼び出し構造の確立」であり、intrinsics 直書きでの最適化は対象外
/// （spec 上「対象外」の項参照）。呼び出しには `unsafe` が必要（[`SimdKernel::dot`]
/// の SAFETY コメント参照）。
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
fn dot_neon<const PADDED_TAIL: bool>(a: &[f32], b: &[f32]) -> f32 {
    const LANES: usize = 4;
    const WIDE: usize = LANES * DOT_ACCUMULATORS;
    if a.len().min(b.len()) >= DOT_MULTI_ACC_MIN_DIM {
        dot_lanes_multi_acc::<LANES, WIDE, PADDED_TAIL>(a, b)
    } else {
        dot_lanes::<LANES, PADDED_TAIL>(a, b)
    }
}

/// x86_64 AVX2+FMA（256 bit・8 レーン）向け内積カーネル。
///
/// `f32::mul_add` で FMA 契約を表現する safe fn。呼び出しには `unsafe` が必要
/// （[`SimdKernel::dot`] の SAFETY コメント参照）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
fn dot_avx2_fma<const PADDED_TAIL: bool>(a: &[f32], b: &[f32]) -> f32 {
    const LANES: usize = 8;
    const WIDE: usize = LANES * DOT_ACCUMULATORS;
    if a.len().min(b.len()) >= DOT_MULTI_ACC_MIN_DIM {
        dot_lanes_multi_acc::<LANES, WIDE, PADDED_TAIL>(a, b)
    } else {
        dot_lanes::<LANES, PADDED_TAIL>(a, b)
    }
}

/// x86_64 AVX-512（`avx512f`。512 bit・16 レーン）向け内積カーネル。
///
/// `avx512f` は FMA を含意する。呼び出しには `unsafe` が必要（[`SimdKernel::dot`]
/// の SAFETY コメント参照）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
fn dot_avx512<const PADDED_TAIL: bool>(a: &[f32], b: &[f32]) -> f32 {
    const LANES: usize = 16;
    const WIDE: usize = LANES * DOT_ACCUMULATORS;
    if a.len().min(b.len()) >= DOT_MULTI_ACC_MIN_DIM {
        dot_lanes_multi_acc::<LANES, WIDE, PADDED_TAIL>(a, b)
    } else {
        dot_lanes::<LANES, PADDED_TAIL>(a, b)
    }
}

/// ISA 別カーネルの共通本体。`LANES` 個ずつのレーンアキュムレータへ `f32::mul_add`
/// （FMA 契約）で積算し、最後にレーン和を固定順（インデックス昇順）で畳む。
/// 添字アクセス（`[]`）は使わず `zip`／イテレータのみで書く
/// （.claude/rules/coding-rust.md）。同一 ISA・同一 LANES・同一 `PADDED_TAIL` では
/// 常に同じ演算順序になるため、`kernel.rs::dot` 経由で呼ぶ全 provider
/// （`CpuScalarProvider`・`ParallelSearchProvider`・バッチ経路・RLS 事前フィルタ）が
/// 同一の丸め誤差で揃う（provider 間の Top-k 整合という既存設計意図を維持する）。
///
/// `as_chunks` の端数（`a_rem`／`b_rem`）は `PADDED_TAIL` の値で処理方式が変わる
/// （Issue #528）が、`PADDED_TAIL` は const ジェネリックのため実行時分岐は生じず、
/// 各 monomorphization 内では固定の演算列になる:
/// - `PADDED_TAIL == false`（現行・[`SimdKernel::dot_with_scalar_tail`]）: 端数を
///   左から右への逐次和（`iter().sum()`）で計算する。
/// - `PADDED_TAIL == true`（[`SimdKernel::dot_with_padded_tail`]）: 端数を
///   [`padded_tail_sum`] へ委譲する（零埋め固定長バッファ経由の分岐なし tail）。
///
/// dim<[`DOT_MULTI_ACC_MIN_DIM`] の入力は常にこの本体を通る（Issue #518 でも本体は
/// 一切変更していない。dim>=[`DOT_MULTI_ACC_MIN_DIM`] のときのみ呼び出し元
/// （`dot_neon`／`dot_avx2_fma`／`dot_avx512`）が [`dot_lanes_multi_acc`] へ分岐する）。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline]
fn dot_lanes<const LANES: usize, const PADDED_TAIL: bool>(a: &[f32], b: &[f32]) -> f32 {
    let len = a.len().min(b.len());
    let a = &a[..len];
    let b = &b[..len];

    let mut lanes = [0f32; LANES];
    let (a_chunks, a_rem) = a.as_chunks::<LANES>();
    let (b_chunks, b_rem) = b.as_chunks::<LANES>();

    for (a_chunk, b_chunk) in a_chunks.iter().zip(b_chunks.iter()) {
        for (lane, (x, y)) in lanes.iter_mut().zip(a_chunk.iter().zip(b_chunk.iter())) {
            *lane = x.mul_add(*y, *lane);
        }
    }

    reduce_lanes::<LANES, PADDED_TAIL>(lanes, a_rem, b_rem)
}

/// [`dot_lanes`] の縮約段（レーン和 → 端数和 → 合算）を切り出した共通関数
/// （Issue #510・TASK-156・CORE-14。ポインタ: `docs/design/dot-kernel-row-block.md`）。
///
/// レーン和・端数和それぞれの計算は [`lane_sum`]／[`tail_sum`] へさらに切り出して
/// あり、x86_64 行ブロックカーネル（[`x86_block4::dot_block4_avx2_fma`]・
/// [`x86_block4::dot_block4_avx512`]）・aarch64 行ブロックカーネル
/// （[`neon_block4::dot_block4_neon`]。Issue #511）は `[f32; LANES]` を経由しない
/// スカラー直接縮約（[`x86_block4::lane_sum8`]／[`lane_sum16`]／
/// [`neon_block4::lane_sum4`]。生成コード検査 `scripts/check_simd_codegen.sh` が
/// 要素ごと挿入命令の再混入を防ぐ。詳細は `docs/design/dot-kernel-row-block.md`
/// 参照）で得たレーン和と、この関数が持つ
/// 端数和 [`tail_sum`] を組み合わせる。[`dot_lanes`]（1 行版）とブロック版が
/// 同一の [`tail_sum`] を共有することで、行ブロック化してもスコアが 1 行版と
/// ビット同一であることを構造的に保証する（`dot_lanes` 自体の挙動・演算順は
/// 本変更で変えない）。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline(always)]
fn reduce_lanes<const LANES: usize, const PADDED_TAIL: bool>(
    lanes: [f32; LANES],
    a_rem: &[f32],
    b_rem: &[f32],
) -> f32 {
    lane_sum(lanes) + tail_sum::<LANES, PADDED_TAIL>(a_rem, b_rem)
}

/// `[f32; LANES]` の左から右への逐次和（`Iterator::sum` の既定実装と同じ
/// `fold(0.0, Add::add)`）。[`dot_lanes`] のレーン和はこの関数を経由する。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline(always)]
fn lane_sum<const LANES: usize>(lanes: [f32; LANES]) -> f32 {
    lanes.iter().sum()
}

/// [`dot_lanes`]・x86_64／aarch64 行ブロックカーネルが共有する端数和
/// （Issue #510・#511）。`PADDED_TAIL` の値による分岐は [`reduce_lanes`] と同一
/// （Issue #528 のドキュメント参照）。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline(always)]
fn tail_sum<const LANES: usize, const PADDED_TAIL: bool>(a_rem: &[f32], b_rem: &[f32]) -> f32 {
    if PADDED_TAIL {
        padded_tail_sum::<LANES>(a_rem, b_rem)
    } else {
        a_rem.iter().zip(b_rem.iter()).map(|(x, y)| x * y).sum()
    }
}

/// 零埋め固定長バッファによる分岐なし tail（Issue #528）。
///
/// `a_rem`／`b_rem`（`LANES` 未満の端数スライス）を `LANES` 長の固定長配列へ詰め、
/// 積を取ってから [`dot_lanes`] の `lane_sum` と同じ `iter().sum()` で縮約する。
/// パディングレーンの積が `s + pad == s`（`s` が符号付きゼロ・±inf・NaN を含む
/// 任意の `f32` でもビット不変）になるよう、`a` 側は `+0.0`・`b` 側は `-0.0` で
/// 埋める（`x * (-0.0) == -0.0`（`x` が有限かつ非ゼロ）・`0.0 * (-0.0) == -0.0`
/// であり、両側を `+0.0` にすると `+0.0` パディングが混ざり `Sum<f32>` の中立元
/// `-0.0` との等価性が崩れるケースが生じるため、片側のみを負にする）。
/// これにより `dot_lanes::<LANES, true>` は `dot_lanes::<LANES, false>` と
/// 全入力でビット同一になる（`tests/isa.rs`・本モジュール unit test で機械検証）。
///
/// `a_rem.len()` と `b_rem.len()` は呼び出し元（[`dot_lanes`]。`a.as_chunks`／
/// `b.as_chunks` を同一の切り詰め済み `len` へ適用した結果）で常に一致するが、
/// 添字アクセスを避けるため `zip` で書き、万一の長さ不一致でも短い方への
/// 切り詰めという `dot_scalar` と同じ意味論を保つ。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline(always)]
fn padded_tail_sum<const LANES: usize>(a_rem: &[f32], b_rem: &[f32]) -> f32 {
    let mut a_pad = [0.0f32; LANES];
    let mut b_pad = [-0.0f32; LANES];
    for (dst, src) in a_pad.iter_mut().zip(a_rem.iter()) {
        *dst = *src;
    }
    for (dst, src) in b_pad.iter_mut().zip(b_rem.iter()) {
        *dst = *src;
    }

    let mut prod = [0.0f32; LANES];
    for (p, (x, y)) in prod.iter_mut().zip(a_pad.iter().zip(b_pad.iter())) {
        *p = x * y;
    }

    prod.iter().sum()
}

// ---------------------------------------------------------------------
// f16 昇格 dot（Issue #514・親 #513。ポインタ: TASK-132・TASK-156・CORE-16）。
//
// `hnsw.rs::NodeVectors::F16`（HNSW 索引ノードの f16 常駐表現。opt-in）が
// 候補生成スコアを計算するための ISA 別カーネル。上記 `SimdKernel`（f32 幅
// ディスパッチ・CORE-11/CORE-12 の決定表が使う）とは別系統の
// トークン・ディスパッチを持つ（`docs/design/simd-intrinsics-adoption.md`
// 決定 3: 「幅」ではなく「機能」の可否を表すため独立 enum・独立 `OnceLock` とする）。
// 索引ヒットの最終スコアは常に `kernel::dot`（f32・既存演算順）で再計算する契約
// （同 ADR 決定 5）のため、本カーネルの ISA 間ビット一致は要求しない
// （同一プロセス・同一 ISA 内での決定性のみを要求する）。
// ---------------------------------------------------------------------

/// x86_64 AVX2+FMA+F16C 対応の実行時確認済みトークン（sealed）。
///
/// [`Avx2FmaToken`] と同じ設計（フィールド private・`pub(crate) fn try_new` のみ）
/// だが、`f16c`（`_mm256_cvtph_ps` に必要）の対応有無も確認する別トークン。
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy)]
pub struct F16cToken(());

#[cfg(target_arch = "x86_64")]
impl F16cToken {
    /// crate 内からのみ呼べる（[`NeonToken::try_new`] と同じ sealed 方針）。
    pub(crate) fn try_new() -> Option<Self> {
        if std::arch::is_x86_feature_detected!("avx2")
            && std::arch::is_x86_feature_detected!("fma")
            && std::arch::is_x86_feature_detected!("f16c")
        {
            Some(F16cToken(()))
        } else {
            None
        }
    }
}

/// aarch64 NEON+FP16（`fcvtl`/`fcvtn` 系変換命令）対応の実行時確認済みトークン
/// （sealed）。
#[cfg(target_arch = "aarch64")]
#[derive(Debug, Clone, Copy)]
pub struct NeonFp16Token(());

#[cfg(target_arch = "aarch64")]
impl NeonFp16Token {
    /// crate 内からのみ呼べる（[`NeonToken::try_new`] と同じ sealed 方針）。
    pub(crate) fn try_new() -> Option<Self> {
        if std::arch::is_aarch64_feature_detected!("neon")
            && std::arch::is_aarch64_feature_detected!("fp16")
        {
            Some(NeonFp16Token(()))
        } else {
            None
        }
    }
}

/// aarch64 NEON dotprod（`neon,dotprod`。Armv8.2-A dot product 拡張。s8×s8→i32
/// の `vdotq_s32`／`sdot v.4s` に必要）対応の実行時確認済みトークン（sealed）。
/// [`I8Kernel::dot_i8`] の整数 i8×i8 dot 専用（Issue #525・親 #520）で、
/// [`NeonFp16Token`] と同じ独立トークン系統（ADR 決定 3）に属する。
#[cfg(target_arch = "aarch64")]
#[derive(Debug, Clone, Copy)]
pub struct NeonDotprodToken(());

#[cfg(target_arch = "aarch64")]
impl NeonDotprodToken {
    /// crate 内からのみ呼べる（[`NeonToken::try_new`] と同じ sealed 方針）。
    /// `CORE-12` の上書き機構不存在方針どおり `is_aarch64_feature_detected!`
    /// 以外のソース（環境変数・設定・feature flag）を一切参照しない
    /// （`tests/isa.rs::isa_source_has_no_external_override_entry_points` が
    /// 本モジュールも走査対象に含めて機械検証する）。
    pub(crate) fn try_new() -> Option<Self> {
        if std::arch::is_aarch64_feature_detected!("neon")
            && std::arch::is_aarch64_feature_detected!("dotprod")
        {
            Some(NeonDotprodToken(()))
        } else {
            None
        }
    }
}

/// f16 昇格 dot（`hnsw.rs::NodeVectors::F16` 専用）の実行時検出済みカーネル。
/// [`SimdKernel`] と同じ sealed トークン方式で、対応 ISA が無い環境では
/// `Scalar`（`f16::f16_bits_to_f32` によるソフトウェア復号）へ fail-closed で
/// 縮退する。
#[derive(Debug, Clone, Copy)]
pub enum F16Kernel {
    /// f16C／NEON+FP16 いずれも未対応（ソフトウェア復号での逐次計算）。
    Scalar,
    /// x86_64 AVX2+FMA+F16C。
    #[cfg(target_arch = "x86_64")]
    F16c(F16cToken),
    /// aarch64 NEON+FP16。
    #[cfg(target_arch = "aarch64")]
    NeonFp16(NeonFp16Token),
}

/// [`F16Kernel`] が使う ISA を表す判別子（`SimdKernel::isa` と同じ「別の判断を
/// 持たない写像」という位置付け）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedF16Isa {
    /// ソフトウェア復号のみ。
    Scalar,
    /// x86_64 AVX2+FMA+F16C。
    F16c,
    /// aarch64 NEON+FP16。
    NeonFp16,
}

impl F16Kernel {
    /// [`DetectedF16Isa`] への純写像。
    pub fn isa(self) -> DetectedF16Isa {
        match self {
            F16Kernel::Scalar => DetectedF16Isa::Scalar,
            #[cfg(target_arch = "x86_64")]
            F16Kernel::F16c(_) => DetectedF16Isa::F16c,
            #[cfg(target_arch = "aarch64")]
            F16Kernel::NeonFp16(_) => DetectedF16Isa::NeonFp16,
        }
    }

    /// `a_bits`（f16 ビット表現。`hnsw.rs::NodeVectors::F16` の索引ノード行）と
    /// `b`（f32。クエリベクトル）の昇格 dot を計算する。長さは短い方へ切り詰める
    /// （[`dot_scalar`] と同じ意味論）。
    pub fn dot_f16(self, a_bits: &[u16], b: &[f32]) -> f32 {
        match self {
            F16Kernel::Scalar => dot_f16_scalar(a_bits, b),
            #[cfg(target_arch = "x86_64")]
            F16Kernel::F16c(_) => {
                // SAFETY: この variant は `F16cToken::try_new` が `avx2`・`fma`・
                // `f16c` の対応を実行時確認できた場合にのみ構築される sealed
                // トークンを保持する（`Avx2FmaToken`/`Avx512Token` 分岐と同じ
                // 構造）。値の存在が CPU 対応の証明であり、`dot_f16_f16c` の
                // `#[target_feature]` 契約を満たす。
                unsafe { dot_f16_f16c(a_bits, b) }
            }
            #[cfg(target_arch = "aarch64")]
            F16Kernel::NeonFp16(_) => {
                // SAFETY: この variant は `NeonFp16Token::try_new` が `neon`・
                // `fp16` の対応を実行時確認できた場合にのみ構築される sealed
                // トークンを保持する。値の存在が CPU 対応の証明であり、
                // `dot_f16_neon_fp16` の `#[target_feature]` 契約を満たす。
                unsafe { dot_f16_neon_fp16(a_bits, b) }
            }
        }
    }
}

/// 優先順（F16C → NeonFp16 → Scalar）で `try_new` を試す（[`detect`] と同じ
/// fail-closed 方針）。
pub fn detect_f16() -> F16Kernel {
    #[cfg(target_arch = "x86_64")]
    {
        if let Some(token) = F16cToken::try_new() {
            return F16Kernel::F16c(token);
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if let Some(token) = NeonFp16Token::try_new() {
            return F16Kernel::NeonFp16(token);
        }
    }
    F16Kernel::Scalar
}

/// プロセス内で 1 回だけ [`detect_f16`] を実行する（[`current`] と同じ方針。
/// `current()` とは独立の `OnceLock` を持ち、f32 幅ディスパッチの決定表
/// （`dispatch.rs`）へは影響しない）。
pub fn current_f16() -> F16Kernel {
    static CURRENT_F16: OnceLock<F16Kernel> = OnceLock::new();
    *CURRENT_F16.get_or_init(detect_f16)
}

/// f16 昇格 dot のスカラー参照実装（`f16::f16_bits_to_f32` で復号してから
/// 左から右への逐次和。ISA 別カーネルとの許容差内一致をテストで確認する）。
pub fn dot_f16_scalar(a_bits: &[u16], b: &[f32]) -> f32 {
    a_bits
        .iter()
        .zip(b.iter())
        .map(|(&bits, &y)| crate::f16::f16_bits_to_f32(bits) * y)
        .sum()
}

/// x86_64 AVX2+FMA+F16C 向け f16 昇格 dot カーネル。
///
/// `as_chunks::<8>()` で得た固定長配列から `_mm_set_epi16`（f16 ビット表現の
/// `set` 構築）→ `_mm256_cvtph_ps`（f16→f32 昇格）、`_mm256_set_ps`（クエリ側）
/// → `_mm256_fmadd_ps` の順に処理する（`docs/design/simd-intrinsics-adoption.md`
/// 決定 1: ポインタ load/store intrinsics 不使用・`set` 構築のみ）。
/// 呼び出しには `unsafe` が必要（[`F16Kernel::dot_f16`] の SAFETY コメント参照）。
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma,f16c")]
fn dot_f16_f16c(a_bits: &[u16], b: &[f32]) -> f32 {
    use std::arch::x86_64::*;

    let len = a_bits.len().min(b.len());
    let a_bits = &a_bits[..len];
    let b = &b[..len];

    let (a_chunks, a_rem) = a_bits.as_chunks::<8>();
    let (b_chunks, b_rem) = b.as_chunks::<8>();

    // `avx2,fma,f16c` は本 fn の `#[target_feature]` で有効化済みのため、
    // 以下の intrinsics 呼び出しは（target_feature 1.1 の規則により）
    // `unsafe` ブロックを要さない safe fn 呼び出しである。引数はいずれも
    // `as_chunks::<8>()` が返す固定長配列（`&[u16; 8]`／`&[f32; 8]`）からの
    // `set` 構築のみで、ポインタ load/store は使わない（決定 1。isa.rs の
    // `unsafe` は [`F16Kernel::dot_f16`] のディスパッチ 2 箇所のみに限定する
    // という設計上の帰結であり、本 fn 自体には `unsafe` ブロックを置かない）。
    let mut acc = _mm256_setzero_ps();
    for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
        let va16 = _mm_set_epi16(
            ac[7] as i16,
            ac[6] as i16,
            ac[5] as i16,
            ac[4] as i16,
            ac[3] as i16,
            ac[2] as i16,
            ac[1] as i16,
            ac[0] as i16,
        );
        let va = _mm256_cvtph_ps(va16);
        let vb = _mm256_set_ps(bc[7], bc[6], bc[5], bc[4], bc[3], bc[2], bc[1], bc[0]);
        acc = _mm256_fmadd_ps(va, vb, acc);
    }

    // 水平和（決定 1・決定 9: `_mm256_extractf128_ps`／`_mm_add_ps`／
    // `_mm_shuffle_ps`／`_mm_add_ss`／`_mm_cvtss_f32` のみを使い、store
    // intrinsics・`transmute` は使わない）。
    let lo = _mm256_castps256_ps128(acc);
    let hi = _mm256_extractf128_ps(acc, 1);
    let sum128 = _mm_add_ps(lo, hi);
    let shuf = _mm_shuffle_ps(sum128, sum128, 0b01_00_11_10);
    let sums = _mm_add_ps(sum128, shuf);
    let shuf2 = _mm_shuffle_ps(sums, sums, 0b00_00_00_01);
    let final_sum = _mm_add_ss(sums, shuf2);
    let lane_sum = _mm_cvtss_f32(final_sum);

    let rem_sum: f32 = a_rem
        .iter()
        .zip(b_rem.iter())
        .map(|(&bits, &y)| crate::f16::f16_bits_to_f32(bits) * y)
        .sum();
    lane_sum + rem_sum
}

/// aarch64 NEON+FP16 向け f16 昇格 dot カーネル。
///
/// `vset_lane_u16` 連鎖（決定 1「第二候補」。`as_chunks::<4>()` の固定長配列から
/// 連続レーンへ順に詰める形は 1 個の `ldr d`＋`fcvtl`（f16→f32 昇格）へ畳み込まれる
/// ことを `--emit asm` で確認済み）で f16 側を、`vsetq_lane_f32` 連鎖で f32 側を
/// 構築し、`vfmaq_f32` で積和する。`#[inline(never)]` は生成コード検査
/// （`scripts/check_simd_codegen.sh`）から独立シンボルとして検出できるようにする
/// ため（決定 2）。呼び出しには `unsafe` が必要
/// （[`F16Kernel::dot_f16`] の SAFETY コメント参照）。
#[cfg(target_arch = "aarch64")]
#[inline(never)]
#[target_feature(enable = "neon,fp16")]
fn dot_f16_neon_fp16(a_bits: &[u16], b: &[f32]) -> f32 {
    use std::arch::aarch64::*;

    let len = a_bits.len().min(b.len());
    let a_bits = &a_bits[..len];
    let b = &b[..len];

    let (a_chunks, a_rem) = a_bits.as_chunks::<4>();
    let (b_chunks, b_rem) = b.as_chunks::<4>();

    // `neon,fp16` は本 fn の `#[target_feature]` で有効化済みのため、以下の
    // intrinsics 呼び出しは `unsafe` ブロックを要さない safe fn 呼び出しである
    // （x86_64 側 `dot_f16_f16c` と同じ注記）。引数はいずれも `as_chunks::<4>()`
    // が返す固定長配列（`&[u16; 4]`／`&[f32; 4]`）からの `set`
    // （`vset_lane_u16`／`vsetq_lane_f32`）構築のみで、ポインタ load/store
    // （`vld1q_*` 等）は使わない（決定 1）。
    let mut acc = vdupq_n_f32(0.0);
    for (ac, bc) in a_chunks.iter().zip(b_chunks.iter()) {
        let mut vh_u16 = vdup_n_u16(0);
        vh_u16 = vset_lane_u16(ac[0], vh_u16, 0);
        vh_u16 = vset_lane_u16(ac[1], vh_u16, 1);
        vh_u16 = vset_lane_u16(ac[2], vh_u16, 2);
        vh_u16 = vset_lane_u16(ac[3], vh_u16, 3);
        let va = vcvt_f32_f16(vreinterpret_f16_u16(vh_u16));

        let mut vb = vdupq_n_f32(0.0);
        vb = vsetq_lane_f32(bc[0], vb, 0);
        vb = vsetq_lane_f32(bc[1], vb, 1);
        vb = vsetq_lane_f32(bc[2], vb, 2);
        vb = vsetq_lane_f32(bc[3], vb, 3);

        acc = vfmaq_f32(acc, va, vb);
    }

    let lane_sum = vaddvq_f32(acc);

    let rem_sum: f32 = a_rem
        .iter()
        .zip(b_rem.iter())
        .map(|(&bits, &y)| crate::f16::f16_bits_to_f32(bits) * y)
        .sum();
    lane_sum + rem_sum
}

/// dim 閾値ディスパッチ（Issue #517/#518）で選ばれる、複数アキュムレータ
/// （`DOT_ACCUMULATORS` 本）版の内積本体。[`dot_lanes`] と同じ「ISA 別カーネルの
/// 共通本体」だが、`WIDE`（`= LANES * DOT_ACCUMULATORS`）個ずつの平坦アキュムレータを
/// 独立に回すことで FMA レイテンシを隠蔽する（`docs/design/
/// dot-kernel-multi-accumulator.md`「設計: 2 段構造」節の採用形をそのまま実装）。
/// `generic_const_exprs` が unstable のため `WIDE` は呼び出し側
/// （`dot_neon`／`dot_avx2_fma`／`dot_avx512`）が計算して渡し、ここでは inline const
/// で整合のみを固定する。
///
/// `#[inline(always)]` にする（`dot_lanes` の `#[inline]` とは意図的に強度を変える）
/// 理由: `#[target_feature]` を付けた呼び出し元 fn の内側で確実にインライン化
/// させないと、この関数が out-of-line なシンボルとしてコンパイルされ
/// `#[target_feature]` の恩恵（AVX2+FMA・AVX-512 命令の生成）を失う。
///
/// 縮約は `dot_lanes` と同じ「固定順・逐次和」（`wide` の和 → `narrow` の和 →
/// 端数和、の順で加算する。段階的な畳み込みは使わない）。`wide` は
/// `[[f32; LANES]; DOT_ACCUMULATORS]` で持ち、`WIDE` チャンクを `LANES` 単位へ
/// `as_chunks` で分けてアキュムレータごとに FMA する（要素ごとの演算順は平坦配列版と
/// 同一）。平坦 `[f32; WIDE]` だと rustc 1.99 の LLVM が 1 要素ずれた SLP を選び
/// ループ毎にレーン挿入が出るため、ベクトル境界を型で固定している。末尾の逐次和の前に
/// `black_box` を挟むのも同じ理由（逐次スカラー和がアキュムレータをスカラー化・
/// 再ベクトル化する誘因になるのを断ち、ループ内は `fmla v.4s` のみにする。
/// ループ脱出時に 1 回だけ作用し、値は変えない）。
/// 添字アクセス（`[]` による単一要素アクセス）は使わず `zip`／イテレータのみで書く
/// （.claude/rules/coding-rust.md）。`narrow` の縮約と端数（`a_rem`／`b_rem`）の
/// 処理は [`dot_lanes`] と同じ [`reduce_lanes`] を経由させ、`PADDED_TAIL`
/// （Issue #528 の tail 方式切り替え）を dim>=[`DOT_MULTI_ACC_MIN_DIM`] でも
/// [`dot_lanes`] と同一の契約で維持する（手書きの逐次和には戻さない）。
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline(always)]
fn dot_lanes_multi_acc<const LANES: usize, const WIDE: usize, const PADDED_TAIL: bool>(
    a: &[f32],
    b: &[f32],
) -> f32 {
    const { assert!(WIDE == LANES * DOT_ACCUMULATORS) };

    let len = a.len().min(b.len());
    let a = &a[..len];
    let b = &b[..len];

    // アキュムレータは LANES 幅のベクトル単位（`[[f32; LANES]; DOT_ACCUMULATORS]`）で持つ。
    // 平坦な `[f32; WIDE]` を要素ごとに回す形だと、rustc 1.99 の LLVM が 1 要素ずれた
    // SLP ベクトル化を選びループ毎にレーン挿入（`mov v.s[k]`）で組み直すため、
    // 各アキュムレータを LANES 単位の chunk で FMA してベクトル境界を固定する。
    let mut wide = [[0f32; LANES]; DOT_ACCUMULATORS];
    let (a_wide_chunks, a_wide_rem) = a.as_chunks::<WIDE>();
    let (b_wide_chunks, b_wide_rem) = b.as_chunks::<WIDE>();

    for (a_chunk, b_chunk) in a_wide_chunks.iter().zip(b_wide_chunks.iter()) {
        let (a_parts, _) = a_chunk.as_chunks::<LANES>();
        let (b_parts, _) = b_chunk.as_chunks::<LANES>();
        for (acc, (a_part, b_part)) in wide.iter_mut().zip(a_parts.iter().zip(b_parts.iter())) {
            for (lane, (x, y)) in acc.iter_mut().zip(a_part.iter().zip(b_part.iter())) {
                *lane = x.mul_add(*y, *lane);
            }
        }
    }

    let mut narrow = [0f32; LANES];
    let (a_narrow_chunks, a_rem) = a_wide_rem.as_chunks::<LANES>();
    let (b_narrow_chunks, b_rem) = b_wide_rem.as_chunks::<LANES>();

    for (a_chunk, b_chunk) in a_narrow_chunks.iter().zip(b_narrow_chunks.iter()) {
        for (lane, (x, y)) in narrow.iter_mut().zip(a_chunk.iter().zip(b_chunk.iter())) {
            *lane = x.mul_add(*y, *lane);
        }
    }

    let wide = core::hint::black_box(wide);
    let wide_sum: f32 = wide.iter().flatten().sum();
    wide_sum + reduce_lanes::<LANES, PADDED_TAIL>(narrow, a_rem, b_rem)
}

// ---------------------------------------------------------------------
// 整数 i8×i8 dot（Issue #522・親 #520・前提 #521。ポインタ: TASK-132・
// TASK-156・CORE-16）。
//
// `hnsw.rs::NodeVectors::I8`（HNSW 索引ノードの SQ8 常駐表現・Issue #521）が
// 候補生成スコアを整数演算で計算するための ISA 別カーネル。[`F16Kernel`] と
// 同じく [`SimdKernel`]（f32 幅ディスパッチ）とは別系統の独立トークン・
// 独立ディスパッチを持つ（ADR 決定 3）。索引ヒットの最終スコアは常に
// `kernel::dot`（f32・アリーナ再計算）で再計算する契約（同 ADR 決定 5）の
// ため、本カーネルの ISA 間ビット一致は要求しない（同一プロセス・同一 ISA
// 内での決定性のみを要求する——ただし整数演算は丸め誤差を持たないため、
// 実際には同一入力に対し全 ISA で厳密に一致する。`tests/isa.rs` が
// `available_i8_kernels()` の全経路でこれを機械検証する）。
// ---------------------------------------------------------------------

/// x86_64 AVX-VNNI（`avx2,avxvnni`）対応の実行時確認済みトークン（sealed）。
/// Alder Lake 以降が持つ、AVX-512 を要さない VEX 符号化 VNNI 命令
/// （`{vex} vpdpbusd`）に対応する。
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy)]
pub struct AvxVnniToken(());

#[cfg(target_arch = "x86_64")]
impl AvxVnniToken {
    /// crate 内からのみ呼べる（[`NeonToken::try_new`] と同じ sealed 方針）。
    pub(crate) fn try_new() -> Option<Self> {
        if std::arch::is_x86_feature_detected!("avx2")
            && std::arch::is_x86_feature_detected!("avxvnni")
        {
            Some(AvxVnniToken(()))
        } else {
            None
        }
    }
}

/// x86_64 AVX-512 VNNI（`avx512f,avx512bw,avx512vnni`）対応の実行時確認済み
/// トークン（sealed）。
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Clone, Copy)]
pub struct Avx512VnniToken(());

#[cfg(target_arch = "x86_64")]
impl Avx512VnniToken {
    /// crate 内からのみ呼べる（[`NeonToken::try_new`] と同じ sealed 方針）。
    pub(crate) fn try_new() -> Option<Self> {
        if std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vnni")
        {
            Some(Avx512VnniToken(()))
        } else {
            None
        }
    }
}

/// 整数 i8×i8 dot の内積が使う 2 種のオペランド（[`crate::sq8::
/// Sq8QueryCodes`] 由来。呼び出し元〔`hnsw.rs::PreparedI8Source`〕が
/// `crate::sq8::prepare_query` の結果からそのまま組み立てる）。契約:
/// `signed.len() == shifted.len()`・`shifted[i] == (signed[i] as i16 + 128)
/// as u8`（`crate::sq8::prepare_query` が満たす）。
#[derive(Debug, Clone, Copy)]
pub struct I8QueryOperands<'a> {
    pub signed: &'a [i8],
    pub shifted: &'a [u8],
}

/// 整数 i8×i8 dot（`hnsw.rs::NodeVectors::I8` 専用）の実行時検出済み
/// カーネル。[`F16Kernel`] と同じ sealed トークン方式で、対応 ISA が無い
/// 環境では `Scalar`（[`dot_i8_scalar`]）へ fail-closed で縮退する。
/// 優先順は x86_64: AVX-512 VNNI → AVX-VNNI → AVX2 i16 widen → Scalar／
/// aarch64: NeonDotprod → Scalar（[`detect_i8`]）。
#[derive(Debug, Clone, Copy)]
pub enum I8Kernel {
    /// いずれの ISA 別カーネルも未対応（スカラー逐次 wrapping 和）。
    Scalar,
    /// x86_64 AVX2（VNNI 非対応。`vpmaddwd` による i16 widen フォールバック）。
    #[cfg(target_arch = "x86_64")]
    Avx2Widen(Avx2FmaToken),
    /// x86_64 AVX-VNNI（256bit）。
    #[cfg(target_arch = "x86_64")]
    AvxVnni(AvxVnniToken),
    /// x86_64 AVX-512 VNNI（512bit）。
    #[cfg(target_arch = "x86_64")]
    Avx512Vnni(Avx512VnniToken),
    /// aarch64 NEON dotprod（`vdotq_s32`。Issue #525）。
    #[cfg(target_arch = "aarch64")]
    NeonDotprod(NeonDotprodToken),
}

/// [`I8Kernel`] が使う ISA を表す判別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedI8Isa {
    /// ソフトウェア（スカラー wrapping 和）のみ。
    Scalar,
    /// x86_64 AVX2 i16 widen フォールバック。
    Avx2Widen,
    /// x86_64 AVX-VNNI（256bit）。
    AvxVnni,
    /// x86_64 AVX-512 VNNI（512bit）。
    Avx512Vnni,
    /// aarch64 NEON dotprod（`vdotq_s32`。Issue #525）。
    NeonDotprod,
}

impl I8Kernel {
    /// [`DetectedI8Isa`] への純写像。
    pub fn isa(self) -> DetectedI8Isa {
        match self {
            I8Kernel::Scalar => DetectedI8Isa::Scalar,
            #[cfg(target_arch = "x86_64")]
            I8Kernel::Avx2Widen(_) => DetectedI8Isa::Avx2Widen,
            #[cfg(target_arch = "x86_64")]
            I8Kernel::AvxVnni(_) => DetectedI8Isa::AvxVnni,
            #[cfg(target_arch = "x86_64")]
            I8Kernel::Avx512Vnni(_) => DetectedI8Isa::Avx512Vnni,
            #[cfg(target_arch = "aarch64")]
            I8Kernel::NeonDotprod(_) => DetectedI8Isa::NeonDotprod,
        }
    }

    /// `codes`（索引ノードの格納コード。`hnsw.rs::NodeVectors::I8` の行）・
    /// `row_sum`（`crate::sq8::row_sums` が凍結時に 1 回計算した `Σ codes`）・
    /// `query`（[`I8QueryOperands`]。`crate::sq8::prepare_query` の結果）から
    /// 符号付き整数内積 `Σ codes_d * query.signed_d` を計算する。
    ///
    /// `codes.len()` が `query.signed.len()`／`query.shifted.len()` と食い
    /// 違う場合（呼び出し元が `dim` 一致を事前検証済みの内部専用パスであり
    /// 通常は到達しない防御的縮退）は `row_sum` に依存しない
    /// [`dot_i8_scalar`] へフォールバックする（`SimdKernel::dot` と同じ
    /// 「短い方への切り詰め」意味論そのものは `dot_i8_scalar` が担う）。
    pub fn dot_i8(self, codes: &[i8], row_sum: i32, query: I8QueryOperands<'_>) -> i32 {
        // aarch64 の `NeonDotprod`（s8×s8 `vdotq_s32`。Issue #525）・
        // `Scalar` は VNNI 系と異なり `row_sum` を参照しない（`NeonDotprod` は
        // `query.signed` をそのまま使う符号付き×符号付き積のため復元が不要。
        // `isa/neon_i8.rs` モジュール doc 参照）。x86_64 以外のビルドでは
        // 下記 x86_64 分岐（`row_sum.wrapping_mul(128)` の実利用）が消えるため
        // `unused_variables` 警告（`make check-cross`）を避ける目的でこの
        // 束縛を維持する。`i32` は `Copy` のため以降の利用を妨げない。
        let _ = row_sum;
        if codes.len() != query.signed.len() || codes.len() != query.shifted.len() {
            return dot_i8_scalar(codes, query.signed);
        }
        match self {
            I8Kernel::Scalar => dot_i8_scalar(codes, query.signed),
            #[cfg(target_arch = "x86_64")]
            I8Kernel::Avx2Widen(_) => {
                // SAFETY: この variant は `Avx2FmaToken::try_new` が `avx2`・
                // `fma` の対応を実行時確認できた場合にのみ構築される sealed
                // トークンを保持する（`avx2+fma ⊇ avx2` のため i16 widen
                // カーネル〔`#[target_feature(enable = "avx2")]`〕の契約を
                // 満たす。新規トークンを増やさずに ADR
                // `docs/design/simd-intrinsics-adoption.md` 決定 3 の表に
                // ある既存トークンを再利用する判断は `docs/design/
                // hnsw-sq8-resident.md`「Issue #522」節参照）。
                unsafe { x86_i8::dot_i8_avx2_widen(codes, query.signed) }
            }
            #[cfg(target_arch = "x86_64")]
            I8Kernel::AvxVnni(_) => {
                // SAFETY: この variant は `AvxVnniToken::try_new` が `avx2`・
                // `avxvnni` の対応を実行時確認できた場合にのみ構築される
                // sealed トークンを保持する。値の存在が CPU 対応の証明であり、
                // `dot_i8_avx_vnni` の `#[target_feature]` 契約を満たす。
                let acc = unsafe { x86_i8::dot_i8_avx_vnni(codes, query.shifted) };
                acc.wrapping_sub(row_sum.wrapping_mul(128))
            }
            #[cfg(target_arch = "x86_64")]
            I8Kernel::Avx512Vnni(_) => {
                // SAFETY: この variant は `Avx512VnniToken::try_new` が
                // `avx512f`・`avx512bw`・`avx512vnni` の対応を実行時確認
                // できた場合にのみ構築される sealed トークンを保持する。
                // 値の存在が CPU 対応の証明であり、`dot_i8_avx512_vnni` の
                // `#[target_feature]` 契約を満たす。
                let acc = unsafe { x86_i8::dot_i8_avx512_vnni(codes, query.shifted) };
                acc.wrapping_sub(row_sum.wrapping_mul(128))
            }
            #[cfg(target_arch = "aarch64")]
            I8Kernel::NeonDotprod(_) => {
                // SAFETY: この variant は `NeonDotprodToken::try_new` が
                // `neon`・`dotprod` の対応を実行時確認できた場合にのみ構築
                // される sealed トークンを保持する。値の存在が CPU 対応の
                // 証明であり、`dot_i8_neon_dotprod` の `#[target_feature]`
                // 契約を満たす。`vdotq_s32` は符号付き×符号付き積のため
                // `row_sum` による復元は不要（`isa/neon_i8.rs` モジュール
                // doc 参照）。
                unsafe { neon_i8::dot_i8_neon_dotprod(codes, query.signed) }
            }
        }
    }
}

/// 優先順（x86_64: Avx512Vnni → AvxVnni → Avx2Widen → Scalar／aarch64:
/// NeonDotprod → Scalar）で `try_new` を試す（[`detect`]・[`detect_f16`] と
/// 同じ fail-closed 方針）。
pub fn detect_i8() -> I8Kernel {
    #[cfg(target_arch = "x86_64")]
    {
        if let Some(token) = Avx512VnniToken::try_new() {
            return I8Kernel::Avx512Vnni(token);
        }
        if let Some(token) = AvxVnniToken::try_new() {
            return I8Kernel::AvxVnni(token);
        }
        if let Some(token) = Avx2FmaToken::try_new() {
            return I8Kernel::Avx2Widen(token);
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if let Some(token) = NeonDotprodToken::try_new() {
            return I8Kernel::NeonDotprod(token);
        }
    }
    I8Kernel::Scalar
}

/// プロセス内で 1 回だけ [`detect_i8`] を実行する（[`current`]・
/// [`current_f16`] と同じ方針。独立の `OnceLock` を持ち、f32 幅ディスパッチ・
/// f16 ディスパッチのいずれへも影響しない）。
pub fn current_i8() -> I8Kernel {
    static CURRENT_I8: OnceLock<I8Kernel> = OnceLock::new();
    *CURRENT_I8.get_or_init(detect_i8)
}

/// このホストで構築可能な [`I8Kernel`] の全 variant（`Scalar` を含む常に
/// 非空の列挙）。ビット同一性テスト（`tests/isa.rs`）が「ディスパッチされた
/// 1 経路」ではなく実際に構築できる全経路を検証するために使う列挙子であり、
/// 上書き機構（CORE-12）ではない——各 variant はこのホストで実際に
/// `try_new()` が `Some` を返したトークンのみから作る。
pub fn available_i8_kernels() -> Vec<I8Kernel> {
    // x86_64／aarch64 以外（本リポは対象外だが cross-check 対象を広げても
    // 壊れないよう防御的に維持）は下の `#[cfg]` ブロックがいずれも完全に
    // 消えるため `kernels` への `push` が一切発生せず、`mut` が
    // cross-check（`make check-cross`）で `unused_mut` 警告になる。
    #[cfg_attr(
        not(any(target_arch = "x86_64", target_arch = "aarch64")),
        allow(unused_mut)
    )]
    let mut kernels = vec![I8Kernel::Scalar];
    #[cfg(target_arch = "x86_64")]
    {
        if let Some(token) = Avx2FmaToken::try_new() {
            kernels.push(I8Kernel::Avx2Widen(token));
        }
        if let Some(token) = AvxVnniToken::try_new() {
            kernels.push(I8Kernel::AvxVnni(token));
        }
        if let Some(token) = Avx512VnniToken::try_new() {
            kernels.push(I8Kernel::Avx512Vnni(token));
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if let Some(token) = NeonDotprodToken::try_new() {
            kernels.push(I8Kernel::NeonDotprod(token));
        }
    }
    kernels
}

/// 整数 i8×i8 dot のスカラー参照実装。`a`・`b` の短い方へ切り詰めたうえで
/// `i32::from` した積を wrapping 加算で逐次和する（`dot_scalar`・
/// `dot_f16_scalar` と同じ「短い方への切り詰め」意味論。整数のため丸め誤差を
/// 持たず、[`I8Kernel::dot_i8`] の全 ISA 別カーネルとビット同一になる）。
///
/// `#[inline(never)]`: コンパイラの自動ベクトル化が `i8`→`i32` の要素ごと
/// 符号拡張を per-lane 命令（x86_64 `punpcklbw`／`punpcklwd`、aarch64
/// `mov v.b[..]`）へ変換すること自体は正当だが、インライン化されると
/// [`I8Kernel::dot_i8`] の `Scalar` 分岐（本来は `unsafe` intrinsics ディス
/// パッチのみを含む薄い関数であるべき）の本体へその命令列が紛れ込み、
/// `scripts/check_simd_codegen.sh` の禁止命令検査（手書き intrinsics
/// カーネルの `set` 構築が gather/stride 由来で per-element insert 命令へ
/// 縮退していないかの検査）が誤検出する（`dot_i8_scalar` 単体は関数名で
/// 除外済み）。独立シンボルとして残すことで誤検出を避ける。
#[inline(never)]
pub fn dot_i8_scalar(a: &[i8], b: &[i8]) -> i32 {
    a.iter().zip(b.iter()).fold(0i32, |acc, (&x, &y)| {
        acc.wrapping_add(i32::from(x).wrapping_mul(i32::from(y)))
    })
}

#[cfg(test)]
mod i8_kernel_tests {
    use super::*;

    /// 決定的な擬似乱数（xorshift64*。`tests/isa.rs` 等と同一アルゴリズム）。
    struct XorShift64Star(u64);
    impl XorShift64Star {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        fn next_i8(&mut self) -> i8 {
            // クランプ済みコード（`sq8.rs` の対称量子化）と同じ範囲
            // [-127, 127] に限定する（`i8::MIN` は本カーネルの入力として
            // 現れない契約——`sq8::quantize_scalar_f64` の不変条件）。
            let v = (self.next_u64() % 255) as i32 - 127;
            v as i8
        }
    }

    fn random_codes(seed: u64, n: usize) -> Vec<i8> {
        let mut rng = XorShift64Star(seed.max(1));
        (0..n).map(|_| rng.next_i8()).collect()
    }

    fn operands_for(codes_len: usize, seed: u64) -> (Vec<i8>, Vec<u8>) {
        let signed = random_codes(seed, codes_len);
        let shifted = signed.iter().map(|&c| (i16::from(c) + 128) as u8).collect();
        (signed, shifted)
    }

    /// [`available_i8_kernels`]（実際に構築できる全経路）×
    /// dim=0..=300（16／32／64 の chunk 境界の端数を網羅）で、
    /// [`I8Kernel::dot_i8`] が [`dot_i8_scalar`]（参照実装で手計算した
    /// `row_sum` 補正込みの値）と `i32` ビット等値になることを固定する。
    #[test]
    fn dispatched_dot_i8_matches_scalar_reference_bit_exact_across_kernels() {
        let kernels = available_i8_kernels();
        assert!(
            kernels.iter().any(|k| matches!(k, I8Kernel::Scalar)),
            "available_i8_kernels must always include Scalar"
        );

        for dim in 0..=300usize {
            let codes = random_codes(0x1234_5678_9abc_def1u64 ^ dim as u64, dim);
            let (signed, shifted) = operands_for(dim, 0x0fed_cba9_8765_4321u64 ^ dim as u64);
            let row_sum: i32 = codes
                .iter()
                .fold(0i32, |acc, &c| acc.wrapping_add(i32::from(c)));
            let expected = dot_i8_scalar(&codes, &signed);

            for &kernel in &kernels {
                let operands = I8QueryOperands {
                    signed: &signed,
                    shifted: &shifted,
                };
                let actual = kernel.dot_i8(&codes, row_sum, operands);
                assert_eq!(
                    actual,
                    expected,
                    "dim={dim} isa={:?} actual={actual} expected={expected}",
                    kernel.isa()
                );
            }
        }
    }

    /// 同一プロセス内での決定性（`current_f16_is_stable_within_process` と
    /// 同じ契約）。
    #[test]
    fn dot_i8_is_deterministic_within_process() {
        let codes = random_codes(0x99, 37);
        let (signed, shifted) = operands_for(37, 0x77);
        let row_sum: i32 = codes
            .iter()
            .fold(0i32, |acc, &c| acc.wrapping_add(i32::from(c)));
        let operands = I8QueryOperands {
            signed: &signed,
            shifted: &shifted,
        };
        let first = current_i8().dot_i8(&codes, row_sum, operands);
        for _ in 0..8 {
            assert_eq!(current_i8().dot_i8(&codes, row_sum, operands), first);
        }
    }

    /// `current_i8()` の単調性（`current_f16_is_stable_within_process` と
    /// 同じ契約）。
    #[test]
    fn current_i8_is_stable_within_process() {
        let first = current_i8().isa();
        for _ in 0..8 {
            assert_eq!(current_i8().isa(), first);
        }
        assert_eq!(detect_i8().isa(), first);
    }

    /// `codes` と `query` の長さが食い違う場合、`row_sum` に依存しない
    /// [`dot_i8_scalar`] と同一の意味論（短い方への切り詰め）になること。
    #[test]
    fn dot_i8_length_mismatch_matches_scalar_semantics() {
        let codes = vec![1i8, 2, 3, 4];
        let signed = vec![5i8, 6];
        let shifted: Vec<u8> = signed.iter().map(|&c| (i16::from(c) + 128) as u8).collect();
        let operands = I8QueryOperands {
            signed: &signed,
            shifted: &shifted,
        };
        assert_eq!(
            current_i8().dot_i8(&codes, 999, operands),
            dot_i8_scalar(&codes, &signed)
        );
        assert_eq!(
            current_i8().dot_i8(
                &[],
                0,
                I8QueryOperands {
                    signed: &[] as &[i8],
                    shifted: &[] as &[u8],
                },
            ),
            0
        );
    }
}

#[cfg(test)]
mod f16_kernel_tests {
    use super::*;

    /// [`F16Kernel::dot_f16`]（実行時検出）と [`dot_f16_scalar`]（参照実装）が
    /// 許容差内で一致すること。dim 0 を含む複数次元・チャンク境界を跨ぐ長さを
    /// 走査する。
    #[test]
    fn dispatched_dot_f16_matches_scalar_reference_within_tolerance() {
        for dim in [0usize, 1, 3, 4, 7, 8, 9, 16, 17, 33, 128, 129] {
            let a_f32: Vec<f32> = (0..dim).map(|i| (i as f32 % 13.0) - 6.0).collect();
            let a_bits: Vec<u16> = a_f32
                .iter()
                .map(|&v| crate::f16::f32_to_f16_bits(v))
                .collect();
            let b: Vec<f32> = (0..dim).map(|i| (i as f32 % 7.0) * 0.5 - 1.5).collect();

            let expected = dot_f16_scalar(&a_bits, &b);
            let actual = current_f16().dot_f16(&a_bits, &b);
            let tolerance = 1e-2 * expected.abs().max(1.0) + 1e-2;
            assert!(
                (actual - expected).abs() <= tolerance,
                "dim={dim} actual={actual} expected={expected}"
            );
        }
    }

    /// 同一プロセス内で `to_bits()` が決定的であること（f32 側 `dot_lanes` の
    /// 決定性契約と同型）。
    #[test]
    fn dot_f16_is_deterministic_within_process() {
        let a_f32: Vec<f32> = (0..37).map(|i| (i as f32) * 0.25 - 4.0).collect();
        let a_bits: Vec<u16> = a_f32
            .iter()
            .map(|&v| crate::f16::f32_to_f16_bits(v))
            .collect();
        let b: Vec<f32> = (0..37).map(|i| (i as f32) * 0.1).collect();

        let first = current_f16().dot_f16(&a_bits, &b);
        for _ in 0..8 {
            assert_eq!(
                current_f16().dot_f16(&a_bits, &b).to_bits(),
                first.to_bits()
            );
        }
    }

    /// `current_f16()` の単調性（`current()` と同じ契約）。
    #[test]
    fn current_f16_is_stable_within_process() {
        let first = current_f16().isa();
        for _ in 0..8 {
            assert_eq!(current_f16().isa(), first);
        }
        assert_eq!(detect_f16().isa(), first);
    }

    /// 長さ不一致・空スライスで [`dot_f16_scalar`] と同一の意味論
    /// （短い方への切り詰め）になること。
    #[test]
    fn dot_f16_length_mismatch_matches_scalar_semantics() {
        let a_f32 = [1.0f32, 2.0, 3.0, 4.0];
        let a_bits: Vec<u16> = a_f32
            .iter()
            .map(|&v| crate::f16::f32_to_f16_bits(v))
            .collect();
        let b = vec![5.0f32, 6.0];

        assert_eq!(
            current_f16().dot_f16(&a_bits, &b),
            dot_f16_scalar(&a_bits, &b)
        );
        assert_eq!(
            current_f16().dot_f16(&[], &a_f32),
            dot_f16_scalar(&[], &a_f32)
        );
        assert_eq!(current_f16().dot_f16(&[] as &[u16], &[] as &[f32]), 0.0f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`try_new`] の否定系（`Some`/`None` いずれの結果でも SAFETY 契約
    /// 「値が存在するなら実際に CPU が対応している」を壊さないこと）と、
    /// [`current`] のプロセス内単調性を確認する。実機の実際の対応有無に依存する
    /// ため、`try_new` が `None` を返す環境では該当分岐を通らないが、
    /// `detect()`／`current()` が常に `Scalar` へ fail-closed で倒れることは
    /// アーキテクチャ非依存に検証できる。
    #[test]
    fn detect_never_panics_and_current_is_stable() {
        let first = current().isa();
        for _ in 0..8 {
            assert_eq!(
                current().isa(),
                first,
                "current() must be stable within process"
            );
        }
        // detect() は毎回照会するが、CPU 対応は実行中に変化しないため current() と
        // 一致するはず。
        assert_eq!(detect().isa(), first);
    }

    /// スカラー参照実装との数値整合。dim 0 を含む複数サイズで許容差内一致を確認する
    /// （このモジュール内 unit テストの範囲。結合テスト側 `tests/isa.rs` がより
    /// 広いサイズ・決定的乱数での回帰を担う）。
    #[test]
    fn current_dot_matches_scalar_reference_within_tolerance() {
        for dim in [0usize, 1, 3, 4, 7, 8, 16, 17, 33] {
            let a: Vec<f32> = (0..dim).map(|i| (i as f32) * 0.5 - 1.0).collect();
            let b: Vec<f32> = (0..dim).map(|i| (i as f32 % 3.0) + 0.25).collect();
            let expected = dot_scalar(&a, &b);
            let actual = current().dot(&a, &b);
            let tolerance = 1e-5 * dot_scalar(&a, &b).abs().max(1.0) + 1e-4;
            assert!(
                (actual - expected).abs() <= tolerance,
                "dim={dim} actual={actual} expected={expected}"
            );
        }
    }

    /// [`dot_lanes`] の `PADDED_TAIL` 両方式（`false`＝現行のスカラー逐次和・
    /// `true`＝[`padded_tail_sum`] 経由の分岐なし tail、Issue #528）が全入力で
    /// ビット同一であることを機械検証する。`dot_lanes` は `#[target_feature]` を
    /// 持たない generic fn（本体は自動ベクトル化に委ねるだけで intrinsics を使わない）
    /// のため、`unsafe` なしに実 ISA へ依存せず直接呼べる。`f32::mul_add` は IEEE
    /// 準拠の正確丸め FMA であり実行 ISA に依存しないため、この unit test の結果は
    /// 実機 ISA（本体テストが動く CI runner の AVX2/AVX-512/NEON 対応有無）に
    /// 関わらず有効な回帰になる。
    ///
    /// NaN 入力は上流（`kernel.rs::KernelError::NonFiniteQuery`）で拒否される契約の
    /// ため対象に含めない（含める場合も、同一 ISA・同一 monomorphization 内であれば
    /// `f32::mul_add` は決定的なので `to_bits()` は一致するはずだが、本テストの
    /// スコープ外とする）。
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn dot_lanes_padded_tail_matches_scalar_tail_bit_exact() {
        // 決定的擬似乱数（xorshift64*。`tests/isa.rs`・`tests/hybrid_recall.rs` 等と
        // 同一アルゴリズム。外部クレート不使用）。
        struct XorShift64Star(u64);
        impl XorShift64Star {
            fn next_u64(&mut self) -> u64 {
                let mut x = self.0;
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                self.0 = x;
                x.wrapping_mul(0x2545_f491_4f6c_dd1d)
            }
            fn next_f32(&mut self) -> f32 {
                let bits = (self.next_u64() >> 40) as u32;
                (bits as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
            }
        }

        fn check<const LANES: usize>(seed: u64) {
            let mut rng = XorShift64Star(seed.max(1));
            // dim 0..=129: LANES ∈ {4, 8, 16} いずれでも複数チャンク＋端数の
            // 組み合わせを網羅する（129 = 8*16+1 = 16*8+1 等、境界を跨ぐ）。
            for dim in 0..=129usize {
                let a: Vec<f32> = (0..dim).map(|_| rng.next_f32()).collect();
                let b: Vec<f32> = (0..dim).map(|_| rng.next_f32()).collect();
                let scalar_tail = dot_lanes::<LANES, false>(&a, &b);
                let padded_tail = dot_lanes::<LANES, true>(&a, &b);
                assert_eq!(
                    scalar_tail.to_bits(),
                    padded_tail.to_bits(),
                    "LANES={LANES} dim={dim} scalar_tail={scalar_tail} padded_tail={padded_tail}"
                );
            }

            // エッジ値集合（符号付きゼロ・微小値・subnormal・境界値）を各要素へ
            // ブロードキャストして dim 1..=LANES*2+1 を走査する。
            let edge_values: [f32; 8] = [
                0.0,
                -0.0,
                1e-30,
                -1e-30,
                1e-25,
                -1e-25,
                f32::MIN_POSITIVE,
                -f32::MIN_POSITIVE,
            ];
            for &value in &edge_values {
                for dim in 1..=(LANES * 2 + 1) {
                    let a = vec![value; dim];
                    let b = vec![value; dim];
                    let scalar_tail = dot_lanes::<LANES, false>(&a, &b);
                    let padded_tail = dot_lanes::<LANES, true>(&a, &b);
                    assert_eq!(
                        scalar_tail.to_bits(),
                        padded_tail.to_bits(),
                        "LANES={LANES} dim={dim} value={value} scalar_tail={scalar_tail} \
                         padded_tail={padded_tail}"
                    );
                }
            }
        }

        for seed in [0x1234_5678_9abc_def1u64, 0x0fed_cba9_8765_4321, 42, 1] {
            check::<4>(seed);
            check::<8>(seed);
            check::<16>(seed);
        }
    }
}
