//! `engine::isa` の結合テスト（TASK-156・対象ビヘイビア: CORE-14）。
//!
//! `dispatch.rs::detect_current_isa()` はもはやコンパイル時 `cfg(target_arch)` だけの
//! 保守的下限検出ではなく、`is_x86_feature_detected!` 等による実行時検出
//! （`isa::current()`）へ委譲する。本ファイルはその配線・決定性・数値整合、および
//! `unsafe`／sealed トークンの構造的な制約（外部上書き機構の不存在・公開コンストラクタ
//! の不存在・`unsafe` の局所化）をソース走査で検査する。
//!
//! `tests/dispatch.rs` の「crate 外から到達できる公開 API だけで検証する」という
//! 位置付けを踏襲し、`SimdKernel` の variant を直接構築することはしない
//! （トークン型・variant はいずれも `pub(crate)`／private フィールドのため、
//! crate 外からは構築できない）。

use engine::isa;

/// `isa::current().isa()` が、`std::arch` の feature 検出マクロ／`cfg(target_arch)`
/// からテスト側で独立に算出した期待値と一致すること。AVX-512 非搭載機では
/// `Avx2Fma` または `Scalar` を返すことを skip ではなく肯定的に検証する。
#[test]
fn detection_matches_std_feature_macros() {
    let expected = expected_isa_from_std_macros();
    assert_eq!(isa::current().isa(), expected);
}

#[cfg(target_arch = "x86_64")]
fn expected_isa_from_std_macros() -> isa::DetectedIsa {
    if std::arch::is_x86_feature_detected!("avx512f") {
        isa::DetectedIsa::Avx512
    } else if std::arch::is_x86_feature_detected!("avx2")
        && std::arch::is_x86_feature_detected!("fma")
    {
        isa::DetectedIsa::Avx2Fma
    } else {
        isa::DetectedIsa::Scalar
    }
}

#[cfg(target_arch = "aarch64")]
fn expected_isa_from_std_macros() -> isa::DetectedIsa {
    if std::arch::is_aarch64_feature_detected!("neon") {
        isa::DetectedIsa::Neon
    } else {
        isa::DetectedIsa::Scalar
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn expected_isa_from_std_macros() -> isa::DetectedIsa {
    isa::DetectedIsa::Scalar
}

/// `current()` の繰り返し呼び出し・複数スレッドからの呼び出しで同一値を返すこと、
/// `detect()`（毎回照会）とも一致すること（プロセス内単調性。`dispatch.rs::
/// select_execution_path` の参照透過性の前提）。
#[test]
fn detection_is_stable_within_process() {
    let baseline = isa::current().isa();

    for _ in 0..8 {
        assert_eq!(isa::current().isa(), baseline);
    }
    assert_eq!(isa::detect().isa(), baseline);

    let handles: Vec<_> = (0..4)
        .map(|_| std::thread::spawn(|| isa::current().isa()))
        .collect();
    for handle in handles {
        assert_eq!(handle.join().expect("thread join"), baseline);
    }
}

/// `dispatch.rs::detect_current_isa()` が `isa::current().isa()` の写像であり、
/// `select_execution_path` が返す `SimdWidth` もそこから決まること（配線の回帰）。
#[test]
fn dispatch_detect_current_isa_reflects_runtime_detection() {
    use engine::dispatch::{
        detect_current_isa, select_execution_path, DetectedIsa, DispatchInput, ExecutionPath,
        SimdWidth,
    };

    assert_eq!(detect_current_isa(), isa::current().isa());

    let expected_width = match detect_current_isa() {
        DetectedIsa::Scalar => SimdWidth::Scalar,
        DetectedIsa::Neon => SimdWidth::W128,
        DetectedIsa::Avx2Fma => SimdWidth::W256,
        DetectedIsa::Avx512 => SimdWidth::W512,
    };

    let input = DispatchInput::for_single_query(8, false).expect("valid input");
    assert_eq!(
        select_execution_path(input),
        Ok(ExecutionPath::CpuSimd {
            width: expected_width
        })
    );
}

// ---------- 決定的擬似乱数（xorshift64*。`tests/hybrid_recall.rs` 等と同一実装。外部クレート不使用） ----------
struct XorShift64Star(u64);

impl XorShift64Star {
    fn new(seed: u64) -> Self {
        XorShift64Star(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn next_f32(&mut self) -> f32 {
        // [-1.0, 1.0) の範囲へ写像する（内積の値域を過度に偏らせないため）。
        let bits = (self.next_u64() >> 40) as u32; // 24 bit
        (bits as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
    }
}

fn random_vec(rng: &mut XorShift64Star, dim: usize) -> Vec<f32> {
    (0..dim).map(|_| rng.next_f32()).collect()
}

/// `isa::current().dot` がスカラー参照実装 [`isa::dot_scalar`] と許容差内で一致する
/// こと。決定的シード RNG で複数次元（0 を含む）を走査する。
#[test]
fn dispatched_dot_matches_scalar_reference_within_tolerance() {
    let dims = [
        0usize, 1, 3, 4, 7, 8, 15, 16, 17, 33, 767, 768, 769, 1000, 1536, 1537,
    ];
    let mut rng = XorShift64Star::new(0x1234_5678_9abc_def1);

    for &dim in &dims {
        let a = random_vec(&mut rng, dim);
        let b = random_vec(&mut rng, dim);

        let expected = isa::dot_scalar(&a, &b);
        let actual = isa::current().dot(&a, &b);

        let magnitude: f32 = a.iter().zip(b.iter()).map(|(x, y)| (x * y).abs()).sum();
        let tolerance = 1e-5 * magnitude + 1e-6;
        assert!(
            (actual - expected).abs() <= tolerance,
            "dim={dim} actual={actual} expected={expected} tolerance={tolerance}"
        );
    }

    // 整数値ベクトルでは丸め誤差が生じないため完全一致することを確認する
    // （FMA・レーン分割があっても整数演算は正確に表現できる範囲内で一致するはず）。
    let a: Vec<f32> = (0..64).map(|i| (i % 7) as f32).collect();
    let b: Vec<f32> = (0..64).map(|i| (i % 5) as f32).collect();
    assert_eq!(isa::current().dot(&a, &b), isa::dot_scalar(&a, &b));
}

/// [`isa::SimdKernel::dot_with_scalar_tail`]（現行のスカラー逐次和 tail）と
/// [`isa::SimdKernel::dot_with_padded_tail`]（零埋め固定長バッファによる分岐なし
/// tail、Issue #528）が dim 0..=129 の全長でビット同一であること。あわせて
/// [`isa::SimdKernel::dot`]（既定経路）が `dot_with_scalar_tail` と一致すること
/// （`DEFAULT_PADDED_TAIL == false` の配線回帰）も確認する。`isa::current().isa()`
/// を assert メッセージへ含め、どの ISA で検証されたかを判別可能にする
/// （実機の AVX2/AVX-512/NEON 対応有無はテスト実行環境依存のため）。
#[test]
fn branchless_tail_matches_scalar_tail_bit_exact_across_dims() {
    let current_isa = isa::current().isa();
    let mut rng = XorShift64Star::new(0x0fed_cba9_8765_4321);

    for dim in 0..=129usize {
        let a = random_vec(&mut rng, dim);
        let b = random_vec(&mut rng, dim);

        let scalar_tail = isa::current().dot_with_scalar_tail(&a, &b);
        let padded_tail = isa::current().dot_with_padded_tail(&a, &b);
        assert_eq!(
            scalar_tail.to_bits(),
            padded_tail.to_bits(),
            "isa={current_isa:?} dim={dim} scalar_tail={scalar_tail} padded_tail={padded_tail}"
        );

        let default_dot = isa::current().dot(&a, &b);
        assert_eq!(
            default_dot.to_bits(),
            scalar_tail.to_bits(),
            "isa={current_isa:?} dim={dim}: SimdKernel::dot must still use the scalar tail \
             (DEFAULT_PADDED_TAIL == false) as production behavior is unchanged by Issue #528"
        );
    }
}

/// 長さ不一致・空スライスで [`isa::dot_scalar`] と同一の意味論（短い方への切り詰め）に
/// なること。
#[test]
fn dispatched_dot_length_mismatch_matches_scalar_semantics() {
    let a = vec![1.0f32, 2.0, 3.0, 4.0];
    let b = vec![5.0f32, 6.0];

    assert_eq!(isa::current().dot(&a, &b), isa::dot_scalar(&a, &b));
    assert_eq!(isa::current().dot(&[], &a), isa::dot_scalar(&[], &a));
    assert_eq!(isa::current().dot(&[] as &[f32], &[] as &[f32]), 0.0f32);
}

/// `isa::I8Kernel::dot_i8`（Issue #522・親 #520・前提 #521。VNNI（512bit／256bit）・
/// i16 widen フォールバック・Issue #525 で追加した aarch64 NEON dotprod の
/// 整数 i8×i8 dot カーネル）が、`isa::available_i8_kernels()`（このホストで
/// 実際に構築できる全 variant——本開発環境（x86_64）では VNNI 非対応のため
/// `Scalar`／`Avx2Widen` のみだが、CI の対応 runner では `AvxVnni`／
/// `Avx512Vnni` も含まれ得る。aarch64 実機（`detect-apple` ジョブ）では
/// `Scalar`／`NeonDotprod` を含む）のいずれでもスカラー参照実装
/// [`isa::dot_i8_scalar`] と `i32` ビット等値になることを、dim 0..=300
/// （chunk 16/32/64 の境界の端数を網羅）・決定的擬似乱数コードで固定する。
/// 整数演算のため丸め誤差を持たず、ISA 間のビット一致を要求できる
/// （`isa.rs` モジュールドキュメンテーションコメント「整数 i8×i8 dot」節参照）。
/// `codes`／`signed`（RNG seed 固定）は arch に依存しない決定的入力であり、
/// [`isa::dot_i8_scalar`] は arch 非依存の純粋な wrapping 整数演算のため、
/// 本テストが x86_64 CI・aarch64 `detect-apple` の双方で green であることが
/// そのまま「x86 版との整数一致（同一入力で同一出力）」の機械的な証跡になる
/// （Issue #525 の受け入れ条件。ADR の想定した個別 golden 定数ハードコード
/// テストではなく、共有シード・共有スカラー参照実装を介した間接証跡とする
/// 判断根拠は `docs/design/hnsw-sq8-resident.md`「Issue #525」節参照）。
#[test]
fn dispatched_dot_i8_matches_scalar_reference_bit_exact_across_kernels() {
    let kernels = isa::available_i8_kernels();
    assert!(
        kernels.iter().any(|k| matches!(k, isa::I8Kernel::Scalar)),
        "available_i8_kernels must always include Scalar"
    );

    let mut rng = XorShift64Star::new(0x0522_1234_5678_9abc);
    for dim in 0..=300usize {
        let codes: Vec<i8> = (0..dim)
            .map(|_| ((rng.next_u64() % 255) as i32 - 127) as i8)
            .collect();
        let signed: Vec<i8> = (0..dim)
            .map(|_| ((rng.next_u64() % 255) as i32 - 127) as i8)
            .collect();
        let shifted: Vec<u8> = signed.iter().map(|&c| (i16::from(c) + 128) as u8).collect();
        let row_sum: i32 = codes
            .iter()
            .fold(0i32, |acc, &c| acc.wrapping_add(i32::from(c)));
        let expected = isa::dot_i8_scalar(&codes, &signed);

        for &kernel in &kernels {
            let operands = isa::I8QueryOperands {
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

/// `isa::I8Kernel::dot_i8` の長さ不一致・空スライスが [`isa::dot_i8_scalar`]
/// と同一の意味論（短い方への切り詰め）になることを固定する
/// （`dispatched_dot_length_mismatch_matches_scalar_semantics` の i8 版）。
#[test]
fn dispatched_dot_i8_length_mismatch_matches_scalar_semantics() {
    let codes = vec![1i8, 2, 3, 4];
    let signed = vec![5i8, 6];
    let shifted: Vec<u8> = signed.iter().map(|&c| (i16::from(c) + 128) as u8).collect();

    assert_eq!(
        isa::current_i8().dot_i8(
            &codes,
            999,
            isa::I8QueryOperands {
                signed: &signed,
                shifted: &shifted,
            },
        ),
        isa::dot_i8_scalar(&codes, &signed)
    );
    assert_eq!(
        isa::current_i8().dot_i8(
            &[],
            0,
            isa::I8QueryOperands {
                signed: &[] as &[i8],
                shifted: &[] as &[u8],
            },
        ),
        0
    );
}

/// `isa::SimdKernel::dot_block4`（Issue #510・#511・TASK-156・CORE-14。行ブロック
/// カーネル）が 1 行版 [`isa::SimdKernel::dot`] とビット同一であることを、
/// 決定的シード RNG で dim 0..=129・768・1000・1536 を走査して検証する
/// （ポインタ: `docs/design/dot-kernel-row-block.md`）。符号付きゼロ・微小値
/// （`f32::MIN_POSITIVE` 近傍）を含む値集合もあわせて検証し、4 行それぞれで
/// 独立した丸め誤差が生じないこと（1 行版と完全に同じ縮約経路を通ること）を
/// 固定する。`isa::current().isa()` が実行時検出した ISA（本開発環境では
/// x86_64 AVX2+FMA／AVX-512、aarch64 実機では Neon）で走るため、NEON 版
/// （`neon_block4::dot_block4_neon`）の実行時ビット同一性の唯一の証跡は
/// `.github/workflows/detect-features.yml` の `detect-apple` ジョブ（Apple
/// Silicon 実機で本テストを実行）が担う。
///
/// dim>=[`isa::DOT_MULTI_ACC_MIN_DIM`]（768）でもこの契約は成立する
/// （Issue #518）。`dot_block4` の intrinsics ブロックカーネル
/// （`x86_block4`／`neon_block4`）自体は [`isa::dot_lanes_multi_acc`] へ
/// 対応させていないが、`isa.rs::SimdKernel::dot_block4_impl` が
/// dim>=[`isa::DOT_MULTI_ACC_MIN_DIM`] を「1 行版 `dot` を 4 回呼ぶ」既存の
/// 非一様長フォールバックへ意図的に合流させているため、`CpuScalarProvider`
/// と `ParallelSearchProvider`（`dot_block4` 経由）の Top-k が dim>=768 でも
/// 食い違わない（`kernel.rs::dot` を共有する全 provider の Top-k 整合という
/// 既存設計意図の維持。詳細は `docs/design/dot-kernel-multi-accumulator.md`
/// 「Issue #518 追記」節参照）。
#[test]
fn dot_block4_matches_single_row_dot_bit_exact_across_dims() {
    let current_isa = isa::current().isa();
    let mut rng = XorShift64Star::new(0x510a_bcde_f012_3456);

    let dims: Vec<usize> = (0..=129usize).chain([768, 1000, 1536]).collect();

    for &dim in &dims {
        let query = random_vec(&mut rng, dim);
        let r0 = random_vec(&mut rng, dim);
        let r1 = random_vec(&mut rng, dim);
        let r2 = random_vec(&mut rng, dim);
        let r3 = random_vec(&mut rng, dim);

        let expected = [
            isa::current().dot(&r0, &query),
            isa::current().dot(&r1, &query),
            isa::current().dot(&r2, &query),
            isa::current().dot(&r3, &query),
        ];
        let actual = isa::current().dot_block4([&r0, &r1, &r2, &r3], &query);

        for i in 0..4 {
            assert_eq!(
                actual[i].to_bits(),
                expected[i].to_bits(),
                "isa={current_isa:?} dim={dim} i={i} actual={} expected={}",
                actual[i],
                expected[i]
            );
        }
    }

    // 符号付きゼロ・微小値（subnormal 近傍）を含むエッジ値集合。
    let edge_values: Vec<f32> = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        f32::MIN_POSITIVE,
        -f32::MIN_POSITIVE,
        f32::EPSILON,
        -f32::EPSILON,
        1e-30,
        -1e-30,
    ];
    let query = edge_values.clone();
    let r0 = edge_values.clone();
    let r1: Vec<f32> = edge_values.iter().rev().copied().collect();
    let r2 = edge_values.clone();
    let r3: Vec<f32> = edge_values.iter().map(|v| v * 2.0).collect();

    let expected = [
        isa::current().dot(&r0, &query),
        isa::current().dot(&r1, &query),
        isa::current().dot(&r2, &query),
        isa::current().dot(&r3, &query),
    ];
    let actual = isa::current().dot_block4([&r0, &r1, &r2, &r3], &query);
    for i in 0..4 {
        assert_eq!(
            actual[i].to_bits(),
            expected[i].to_bits(),
            "isa={current_isa:?} edge-values i={i} actual={} expected={}",
            actual[i],
            expected[i]
        );
    }
}

/// dim 閾値ディスパッチ（Issue #517/#518）: dim<[`isa::DOT_MULTI_ACC_MIN_DIM`] の
/// 経路が [`isa::dot_scalar`]（参照実装）と厳密ビット一致であることを、整数値
/// ベクトル（乗算・加算がいずれの順序でも `f32` として厳密表現できる値域）で
/// 確認する。ここでの厳密一致は「実装のどこかの過去バージョンと同一である」こと
/// ではなく「dim<閾値は multi-acc 経路（[`isa::DOT_MULTI_ACC_MIN_DIM`] 以上でのみ
/// 到達する [`dot_multi_acc_threshold_boundary_matches_scalar_exactly`] の経路）を
/// 一切通らず、常に正しい内積を返す」ことを実オラクル（`dot_scalar`）と突き合わせて
/// 固定する（`current().dot(&a, &b)` を 2 回呼んで自分自身と比較するだけの
/// vacuous なテストにしないため）。
#[test]
fn dot_below_threshold_dispatch_is_unaffected_by_multi_acc_addition() {
    let dims = [0usize, 1, 7, 8, 100, 128, 384, 767];

    for &dim in &dims {
        assert!(
            dim < isa::DOT_MULTI_ACC_MIN_DIM,
            "test setup: dim must be below threshold"
        );
        let a: Vec<f32> = (0..dim).map(|i| (i % 11) as f32).collect();
        let b: Vec<f32> = (0..dim).map(|i| (i % 13) as f32).collect();

        let expected = isa::dot_scalar(&a, &b);
        let actual = isa::current().dot(&a, &b);
        assert_eq!(
            actual, expected,
            "dim={dim} integer-valued dot must match scalar reference exactly \
             (dim below DOT_MULTI_ACC_MIN_DIM must never take the multi-acc path)"
        );
    }
}

/// 4 行と `query` の長さが 1 つでも異なる場合、[`isa::SimdKernel::dot_block4`] が
/// 高速経路（intrinsics ブロックカーネル）へ入らず、1 行版 `dot` を 4 回呼ぶ
/// 縮退経路と一致すること（`isa.rs::SimdKernel::dot_block4_impl` の
/// `uniform_len` 判定の回帰。production では `parallel_search.rs::search_range`
/// が常に 4 行と `query` の長さを揃えて呼ぶため、この経路は主に安全側の
/// フォールバックとして機能する）。
#[test]
fn dot_block4_falls_back_to_single_row_dot_when_lengths_are_not_uniform() {
    let query = vec![1.0f32, 2.0, 3.0, 4.0];
    let r0 = vec![1.0f32, 0.0, 0.0, 0.0]; // query と同じ長さ
    let r1 = vec![1.0f32, 0.0, 0.0]; // 1 要素短い
    let r2 = vec![1.0f32, 0.0, 0.0, 0.0, 0.0]; // 1 要素長い
    let r3: Vec<f32> = Vec::new(); // 空

    let expected = [
        isa::current().dot(&r0, &query),
        isa::current().dot(&r1, &query),
        isa::current().dot(&r2, &query),
        isa::current().dot(&r3, &query),
    ];
    let actual = isa::current().dot_block4([&r0, &r1, &r2, &r3], &query);
    assert_eq!(actual, expected);
}

/// 閾値境界（767→旧経路・768→新経路）を整数ベクトルで確認する。整数値の内積は
/// 加算順序に依らず厳密に一致するため（Recall フィクスチャが dim 800/1000 の
/// one-hot 系ベクトルで構造的に不変である論拠と同じ）、新旧どちらの経路を通っても
/// `isa::dot_scalar` と完全一致するはずである。
#[test]
fn dot_multi_acc_threshold_boundary_matches_scalar_exactly() {
    assert_eq!(isa::DOT_MULTI_ACC_MIN_DIM, 768);

    for &dim in &[767usize, 768, 769, 800, 1000, 1536, 1537] {
        let a: Vec<f32> = (0..dim).map(|i| (i % 7) as f32).collect();
        let b: Vec<f32> = (0..dim).map(|i| (i % 5) as f32).collect();

        let expected = isa::dot_scalar(&a, &b);
        let actual = isa::current().dot(&a, &b);
        assert_eq!(
            actual, expected,
            "dim={dim} integer-valued dot must match scalar reference exactly"
        );
    }
}

/// CORE-14: 検出結果への外部入力上書き機構（環境変数・設定ファイル読み取り等）が
/// ソース上に存在しないことを確認する（`tests/dispatch.rs::
/// dispatch_source_has_no_external_override_entry_points` と同じ禁止トークン集合）。
#[test]
fn isa_source_has_no_external_override_entry_points() {
    // Issue #510・#511・#525: `isa/x86_block4.rs`（cfg(x86_64) サブモジュール）・
    // `isa/neon_block4.rs`（cfg(aarch64) サブモジュール）・`isa/neon_i8.rs`
    // （cfg(aarch64) サブモジュール）も同じ禁止トークン集合で走査する
    // （`isa.rs` 本体からモジュール分割しても CORE-12 の「上書き機構の不存在」
    // 検査が抜け穴にならないようにするため）。`include_str!` は `cfg` に
    // 依らずファイルの存在のみ要求するため、x86_64 ホストのテストでも
    // `neon_block4.rs`／`neon_i8.rs` を走査できる。
    let sources = [
        include_str!("../src/isa.rs"),
        include_str!("../src/isa/x86_block4.rs"),
        include_str!("../src/isa/neon_block4.rs"),
        include_str!("../src/isa/x86_i8.rs"),
        include_str!("../src/isa/neon_i8.rs"),
    ];

    let forbidden_tokens = [
        "std::env",
        "env::var",
        "env::var_os",
        "std::fs",
        "read_to_string",
        "File::open",
        "option_env!",
    ];

    for source in sources {
        for token in forbidden_tokens {
            assert!(
                !source.contains(token),
                "isa module must not contain external override entry point token: {token}"
            );
        }
    }
}

/// `isa::current().l1` がスカラー参照実装 [`isa::l1_scalar`] と許容差内で一致すること
/// （Issue #1988・SQL-33）。全項が非負なので和そのものを magnitude とする。
#[test]
fn dispatched_l1_matches_scalar_reference_within_tolerance() {
    let dims = [
        0usize, 1, 3, 4, 7, 8, 15, 16, 17, 33, 767, 768, 769, 1000, 1536, 1537,
    ];
    let mut rng = XorShift64Star::new(0x0bad_cafe_1988_0001);
    for &dim in &dims {
        let a = random_vec(&mut rng, dim);
        let b = random_vec(&mut rng, dim);
        let expected = isa::l1_scalar(&a, &b);
        let actual = isa::current().l1(&a, &b);
        let tolerance = 1e-5 * expected + 1e-6;
        assert!(
            (actual - expected).abs() <= tolerance,
            "dim={dim} actual={actual} expected={expected} tolerance={tolerance}"
        );
        // 同一プロセス内の決定性（同じ入力はビット同一）。
        assert_eq!(actual.to_bits(), isa::current().l1(&a, &b).to_bits());
    }
}

/// 整数値ベクトルでは丸め誤差が生じないため `l1` が scalar とビット完全一致し、
/// 長さ不一致では短い方へ切り詰める意味論も一致すること（Issue #1988・SQL-33）。
#[test]
fn dispatched_l1_exact_on_integers_and_truncates_mismatched_lengths() {
    for dim in [0usize, 1, 5, 8, 16, 17, 64, 129, 1000] {
        let a: Vec<f32> = (0..dim).map(|i| (i % 7) as f32 - 3.0).collect();
        let b: Vec<f32> = (0..dim).map(|i| (i % 5) as f32).collect();
        assert_eq!(
            isa::current().l1(&a, &b),
            isa::l1_scalar(&a, &b),
            "dim={dim}"
        );
    }
    assert_eq!(isa::l1_scalar(&[1.0, 2.0, 3.0], &[4.0, 0.0, 3.0]), 5.0);
    let a: Vec<f32> = (0..50).map(|i| i as f32).collect();
    let b: Vec<f32> = (0..37).map(|i| (i * 2) as f32).collect();
    assert_eq!(isa::current().l1(&a, &b), isa::l1_scalar(&a, &b));
    assert_eq!(isa::current().l1(&b, &a), isa::l1_scalar(&b, &a));
}

/// `isa::current().l2_sq` が scalar 参照 [`isa::l2_sq_scalar`] と相対許容差内で一致すること
/// （Issue #1983・SQL-33）。FMA の単一丸めと scalar の二重丸めの差を `1e-5 * expected + 1e-6`
/// で許容する（全項非負なので和そのものを magnitude とする）。
#[test]
fn dispatched_l2_sq_matches_scalar_reference_within_tolerance() {
    let dims = [
        0usize, 1, 3, 4, 7, 8, 15, 16, 17, 33, 767, 768, 769, 1000, 1536, 1537,
    ];
    let mut rng = XorShift64Star::new(0x0bad_cafe_1983_0001);
    for &dim in &dims {
        let a = random_vec(&mut rng, dim);
        let b = random_vec(&mut rng, dim);
        let expected = isa::l2_sq_scalar(&a, &b);
        let actual = isa::current().l2_sq(&a, &b);
        let tolerance = 1e-5 * expected + 1e-6;
        assert!(
            (actual - expected).abs() <= tolerance,
            "dim={dim} actual={actual} expected={expected} tolerance={tolerance}"
        );
        assert_eq!(actual.to_bits(), isa::current().l2_sq(&a, &b).to_bits());
    }
}

/// 整数値ベクトル（各項の二乗和が 2^24 未満）では丸めが生じないため `l2_sq` が scalar と
/// ビット一致し、長さ不一致では短い方へ切り詰める意味論も一致すること（Issue #1983）。
#[test]
fn dispatched_l2_sq_exact_on_integers_and_truncates_mismatched_lengths() {
    for dim in [0usize, 1, 5, 8, 16, 17, 64, 129, 1000] {
        let a: Vec<f32> = (0..dim).map(|i| (i % 7) as f32 - 3.0).collect();
        let b: Vec<f32> = (0..dim).map(|i| (i % 5) as f32).collect();
        assert_eq!(
            isa::current().l2_sq(&a, &b).to_bits(),
            isa::l2_sq_scalar(&a, &b).to_bits(),
            "dim={dim}"
        );
    }
    assert_eq!(isa::l2_sq_scalar(&[1.0, 2.0, 3.0], &[4.0, 0.0, 3.0]), 13.0);
    let a: Vec<f32> = (0..50).map(|i| i as f32).collect();
    let b: Vec<f32> = (0..37).map(|i| (i * 2) as f32).collect();
    assert_eq!(isa::current().l2_sq(&a, &b), isa::l2_sq_scalar(&a, &b));
    assert_eq!(isa::current().l2_sq(&b, &a), isa::l2_sq_scalar(&b, &a));
}

/// `l2_sq_block4` が 1 行版 `l2_sq` と全次元でビット同一であること（符号付きゼロ・
/// 微小値を含む。Issue #1983・SQL-33）。
#[test]
fn l2_sq_block4_matches_single_row_bit_exact_across_dims() {
    let current_isa = isa::current().isa();
    let mut rng = XorShift64Star::new(0x1983_abcd_ef01_2345);
    let dims: Vec<usize> = (0..=129usize).chain([768, 1000, 1536]).collect();
    for &dim in &dims {
        let query = random_vec(&mut rng, dim);
        let mut rows: Vec<Vec<f32>> = (0..4).map(|_| random_vec(&mut rng, dim)).collect();
        // 境界値: 符号付きゼロ・微小値・クエリと完全一致する行。
        if let Some(x) = rows[1].first_mut() {
            *x = -0.0;
        }
        if let Some(x) = rows[2].last_mut() {
            *x = 1e-30;
        }
        rows[3] = query.clone();
        let expected: Vec<f32> = rows
            .iter()
            .map(|r| isa::current().l2_sq(r, &query))
            .collect();
        let actual = isa::current().l2_sq_block4([&rows[0], &rows[1], &rows[2], &rows[3]], &query);
        for i in 0..4 {
            assert_eq!(
                actual[i].to_bits(),
                expected[i].to_bits(),
                "isa={current_isa:?} dim={dim} i={i}"
            );
        }
    }
}

/// 4 行の長さが不揃いでも `l2_sq_block4` は 1 行版 4 回と一致する（縮退経路。Issue #1983）。
#[test]
fn l2_sq_block4_falls_back_when_lengths_are_not_uniform() {
    let mut rng = XorShift64Star::new(0x1983_0000_0000_0042);
    let query = random_vec(&mut rng, 37);
    let r0 = random_vec(&mut rng, 37);
    let r1 = random_vec(&mut rng, 20);
    let r2 = random_vec(&mut rng, 50);
    let r3 = random_vec(&mut rng, 0);
    let actual = isa::current().l2_sq_block4([&r0, &r1, &r2, &r3], &query);
    for (i, r) in [&r0, &r1, &r2, &r3].into_iter().enumerate() {
        assert_eq!(
            actual[i].to_bits(),
            isa::current().l2_sq(r, &query).to_bits(),
            "i={i}"
        );
    }
}

/// 整数値 fixture の L2 Top-k（距離昇順・同点 id 昇順）が、ISA 実装（`l2_sq`＋`l2_sq_block4`）
/// と scalar 参照で id 列・距離ビットとも完全一致すること（Issue #1983・SQL-33。
/// 「ISA 間で同一の Top-k」を同一プロセスで検証できる形に落とした証跡。x86 側の SIMD 実装
/// 〔Issue #1982〕も同じテストを通る）。値域が小さく同点が頻出する fixture にしている。
#[test]
fn l2_top_k_matches_scalar_on_integer_fixture() {
    let mut rng = XorShift64Star::new(0x1983_7007_c0de_0001);
    let mut int_vec = |dim: usize| -> Vec<f32> {
        (0..dim)
            .map(|_| ((rng.next_u64() >> 33) % 17) as f32 - 8.0)
            .collect()
    };
    for dim in [1usize, 17, 128, 1536] {
        let rows: Vec<Vec<f32>> = (0..4 * 25 + 3).map(|_| int_vec(dim)).collect();
        let query = int_vec(dim);

        let scalar: Vec<(f32, usize)> = rows
            .iter()
            .enumerate()
            .map(|(id, r)| (isa::l2_sq_scalar(r, &query), id))
            .collect();

        let kernel = isa::current();
        let mut simd: Vec<(f32, usize)> = Vec::with_capacity(rows.len());
        let (blocks, remainder) = rows.as_chunks::<4>();
        let mut id = 0usize;
        for [r0, r1, r2, r3] in blocks {
            let d = kernel.l2_sq_block4([r0, r1, r2, r3], &query);
            for v in d {
                simd.push((v, id));
                id += 1;
            }
        }
        for r in remainder {
            simd.push((kernel.l2_sq(r, &query), id));
            id += 1;
        }

        let top_k = |mut v: Vec<(f32, usize)>| -> Vec<(u32, usize)> {
            v.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
            v.into_iter()
                .take(10)
                .map(|(d, i)| (d.to_bits(), i))
                .collect()
        };
        assert_eq!(top_k(simd), top_k(scalar), "dim={dim}");
    }
}

/// `unsafe` が `isa.rs` 以外の `crates/engine/src/**/*.rs` に存在しないこと、
/// `isa.rs` 内の各 `unsafe {` 直前数行に `SAFETY:` があることをソース走査で確認する
/// （AGENTS.md P1「`unsafe` の立証」。sealed トークン所持を根拠とする 2 箇所
/// （AVX2+FMA・AVX-512）以外に `unsafe` を増やさないための回帰）。
#[test]
fn unsafe_is_confined_to_isa_module_with_safety_comments() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut rs_files = Vec::new();
    collect_rs_files(&src_dir, &mut rs_files);
    assert!(!rs_files.is_empty(), "no .rs files found under src/");

    for path in &rs_files {
        let content = std::fs::read_to_string(path).expect("read source file");
        // Issue #510: `isa/x86_block4.rs`（`isa.rs` の cfg(x86_64) サブモジュール）は
        // `unsafe` を持たない safe fn のみで構成する契約（ADR
        // `docs/design/simd-intrinsics-adoption.md` 決定 1）だが、それはこの検査を
        // 弱める理由にはならない。`unsafe` を許すのは sealed トークン所持を根拠に
        // 検証済みの `isa.rs` 本体のみとし、`isa/` 配下のサブモジュールへ `unsafe`
        // が紛れ込んだ場合はこの検査で検出できるよう除外範囲を `isa.rs` 単体に限定
        // する（codex-review 指摘対応。ディレクトリ一致による除外は
        // `isa/x86_block4.rs` への `unsafe` 追加を無検査で通してしまうため撤回）。
        let is_isa_module = path.file_name().and_then(|n| n.to_str()) == Some("isa.rs");

        if !is_isa_module {
            assert!(
                !content.contains("unsafe "),
                "unsafe must be confined to isa.rs, found a token in {}",
                path.display()
            );
            assert!(
                !content.contains("unsafe{"),
                "unsafe must be confined to isa.rs, found a token in {}",
                path.display()
            );
        }
    }

    let isa_path = src_dir.join("isa.rs");
    let isa_source = std::fs::read_to_string(&isa_path).expect("read isa.rs");
    let lines: Vec<&str> = isa_source.lines().collect();
    let mut unsafe_block_count = 0usize;

    for (idx, line) in lines.iter().enumerate() {
        if line.contains("unsafe {") {
            unsafe_block_count += 1;
            // 直前 10 行以内に `SAFETY:` があることを確認する。
            let start = idx.saturating_sub(10);
            let has_safety_comment = lines[start..idx].iter().any(|l| l.contains("SAFETY:"));
            assert!(
                has_safety_comment,
                "unsafe block at isa.rs line {} has no preceding SAFETY: comment",
                idx + 1
            );
        }
    }

    // ソーステキスト上には NEON・AVX2+FMA・AVX-512 の `dot` ディスパッチ 3 箇所に加え、
    // Issue #510（TASK-156・CORE-14）で追加した `dot_block4` の AVX2+FMA・AVX-512
    // ディスパッチ 2 箇所、Issue #514 で追加した F16c/NeonFp16 の `dot_f16`
    // ディスパッチ 2 箇所、Issue #511（TASK-156・CORE-14）で追加した `dot_block4`
    // の Neon ディスパッチ 1 箇所、Issue #522（TASK-156・CORE-14）で追加した
    // `dot_i8` の Avx2Widen・AvxVnni・Avx512Vnni ディスパッチ 3 箇所、Issue #525
    // （TASK-156・CORE-14）で追加した `dot_i8` の NeonDotprod ディスパッチ 1 箇所の
    // 計 12 箇所の `unsafe {` が現れる（実際のビルドで有効になるのは対象 arch の
    // 分岐のみだが、`cfg` 行はソース上に残ったまま走査されるため、arch に
    // 依存せず常に 12 を期待できる）。Issue #1988（SQL-33）で追加した `l1` の
    // Neon・Avx2Fma・Avx512 ディスパッチ 3 箇所を加えて 15 箇所、Issue #1983（SQL-33）で
    // 追加した `l2_sq`／`l2_sq_block4` の Neon ディスパッチ 2 箇所、Issue #2022（TABLE-48）で
    // 追加した popcount の hamming／jaccard_counts × Neon・Avx2・Avx512Vpopcntdq
    // ディスパッチ 6 箇所を加えて計 23 箇所となる。
    assert_eq!(
        unsafe_block_count, 23,
        "expected exactly 23 `unsafe {{` blocks in isa.rs (Issue #2022 popcount dispatch x6 + Issue #1983 l2_sq/l2_sq_block4 Neon dispatch x2 + Issue #1988 l1 dispatch x3 + Neon/Avx2Fma/Avx512 dot dispatch \
         + Avx2Fma/Avx512 dot_block4 dispatch + F16c/NeonFp16 dot_f16 dispatch added by \
         Issue #514 + Neon dot_block4 dispatch added by Issue #511 + Avx2Widen/AvxVnni/ \
         Avx512Vnni dot_i8 dispatch added by Issue #522 + NeonDotprod dot_i8 dispatch added \
         by Issue #525)"
    );
}

fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// トークン型（`NeonToken`／`Avx2FmaToken`／`Avx512Token`）に `pub fn new`／
/// `pub fn try_new` が存在せず、`(())` 形式（unit struct・単一 private フィールド）で
/// あることをソース走査で確認する（sealed 方針の回帰。crate 外から任意のトークンを
/// 構築できないことの構造的な担保）。
#[test]
fn token_types_have_no_public_constructor() {
    let source = include_str!("../src/isa.rs");

    assert!(
        !source.contains("pub fn new"),
        "isa.rs must not expose a public constructor for token types"
    );
    assert!(
        !source.contains("pub fn try_new"),
        "isa.rs must not expose a public try_new constructor for token types"
    );

    for token in [
        "NeonToken",
        "Avx2FmaToken",
        "Avx512Token",
        "F16cToken",
        "NeonFp16Token",
    ] {
        assert!(
            source.contains(&format!("struct {token}(())")),
            "{token} must be defined as a unit-field tuple struct `{token}(())`"
        );
    }
}

// ---------- f16 昇格 dot（Issue #514・親 #513。ポインタ: TASK-132・TASK-156・CORE-16） ----------
//
// `isa::F16Kernel`（`hnsw.rs::NodeVectors::F16` 専用のディスパッチ）の結合テスト。
// `SimdKernel`（f32 幅ディスパッチ）の既存テストと同じ「crate 外から到達できる
// 公開 API だけで検証する」方針を踏襲する。

/// `isa::current_f16().isa()` が、`std::arch` の feature 検出マクロから
/// テスト側で独立に算出した期待値と一致すること。
#[test]
fn f16_detection_matches_std_feature_macros() {
    let expected = expected_f16_isa_from_std_macros();
    assert_eq!(isa::current_f16().isa(), expected);
}

#[cfg(target_arch = "x86_64")]
fn expected_f16_isa_from_std_macros() -> isa::DetectedF16Isa {
    if std::arch::is_x86_feature_detected!("avx2")
        && std::arch::is_x86_feature_detected!("fma")
        && std::arch::is_x86_feature_detected!("f16c")
    {
        isa::DetectedF16Isa::F16c
    } else {
        isa::DetectedF16Isa::Scalar
    }
}

#[cfg(target_arch = "aarch64")]
fn expected_f16_isa_from_std_macros() -> isa::DetectedF16Isa {
    if std::arch::is_aarch64_feature_detected!("neon")
        && std::arch::is_aarch64_feature_detected!("fp16")
    {
        isa::DetectedF16Isa::NeonFp16
    } else {
        isa::DetectedF16Isa::Scalar
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn expected_f16_isa_from_std_macros() -> isa::DetectedF16Isa {
    isa::DetectedF16Isa::Scalar
}

/// `isa::current_f16().dot_f16` が `isa::dot_f16_scalar`（参照実装）と許容差内で
/// 一致すること。決定的シード RNG で複数次元（0・チャンク境界を跨ぐ長さを含む）を
/// 走査する。f16 の丸め誤差を許容するため許容差は f32 側 `dispatched_dot_matches_
/// scalar_reference_within_tolerance` より緩める。
#[test]
fn dispatched_dot_f16_matches_scalar_reference_within_tolerance() {
    let dims = [0usize, 1, 3, 4, 7, 8, 15, 16, 17, 33, 128, 129];
    let mut rng = XorShift64Star::new(0x2468_ace0_1357_9bdf);

    for &dim in &dims {
        let a_f32 = random_vec(&mut rng, dim);
        let a_bits: Vec<u16> = a_f32
            .iter()
            .map(|&v| {
                let packed = engine::batch_search::pack_f16x2(v, 0.0);
                let bits = packed & 0xFFFF;
                bits as u16
            })
            .collect();
        let b = random_vec(&mut rng, dim);

        let expected = isa::dot_f16_scalar(&a_bits, &b);
        let actual = isa::current_f16().dot_f16(&a_bits, &b);

        let magnitude: f32 = a_f32.iter().zip(b.iter()).map(|(x, y)| (x * y).abs()).sum();
        let tolerance = 1e-2 * magnitude + 1e-2;
        assert!(
            (actual - expected).abs() <= tolerance,
            "dim={dim} actual={actual} expected={expected} tolerance={tolerance}"
        );
    }
}

/// `isa::current_f16()` の単調性（`isa::current()` と同じ契約。プロセス内で
/// 何度呼んでも同一 ISA を返す）。
#[test]
fn f16_detection_is_stable_within_process() {
    let baseline = isa::current_f16().isa();
    for _ in 0..8 {
        assert_eq!(isa::current_f16().isa(), baseline);
    }
    assert_eq!(isa::detect_f16().isa(), baseline);
}

// ---------- popcount カーネル（Issue #2022・親 #2020・TABLE-48 ポインタ） ----------

/// 1 ビットずつ数える素朴な参照実装（`(hamming, intersection, union)`）。
/// 8 バイトまとめ数えのスカラー版を参照実装として扱わないための独立実装。
fn popcount_bit_reference(a: &[u8], b: &[u8]) -> (u64, u64, u64) {
    let (mut x, mut i, mut u) = (0u64, 0u64, 0u64);
    for (p, q) in a.iter().zip(b) {
        for bit in 0..8 {
            let (p, q) = ((p >> (7 - bit)) & 1 == 1, (q >> (7 - bit)) & 1 == 1);
            x += u64::from(p != q);
            i += u64::from(p && q);
            u += u64::from(p || q);
        }
    }
    (x, i, u)
}

fn popcount_cases() -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut rng = XorShift64Star::new(0x2022_0b17);
    let lens = [
        0usize, 1, 7, 8, 9, 15, 16, 31, 32, 33, 63, 64, 65, 127, 128, 129, 1000, 1024, 1536, 8000,
    ];
    let mut cases = Vec::new();
    for &len in &lens {
        let rand = |rng: &mut XorShift64Star| -> Vec<u8> {
            (0..len).map(|_| (rng.next_u64() >> 32) as u8).collect()
        };
        cases.push((rand(&mut rng), rand(&mut rng)));
        cases.push((vec![0u8; len], vec![0u8; len]));
        cases.push((vec![0xFFu8; len], vec![0xFFu8; len]));
        cases.push((vec![0xFFu8; len], vec![0u8; len]));
        cases.push((vec![0xAAu8; len], vec![0x55u8; len]));
    }
    cases
}

/// `current_popcount().isa()` が std の検出マクロから独立に求めた期待値と一致する。
#[test]
fn popcount_detection_matches_std_feature_macros() {
    #[cfg(target_arch = "x86_64")]
    let expected = if std::arch::is_x86_feature_detected!("avx512f")
        && std::arch::is_x86_feature_detected!("avx512vpopcntdq")
    {
        isa::DetectedPopcountIsa::Avx512Vpopcntdq
    } else if std::arch::is_x86_feature_detected!("avx2")
        && std::arch::is_x86_feature_detected!("fma")
    {
        isa::DetectedPopcountIsa::Avx2
    } else {
        isa::DetectedPopcountIsa::Scalar
    };
    #[cfg(target_arch = "aarch64")]
    let expected = if std::arch::is_aarch64_feature_detected!("neon") {
        isa::DetectedPopcountIsa::Neon
    } else {
        isa::DetectedPopcountIsa::Scalar
    };
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let expected = isa::DetectedPopcountIsa::Scalar;
    assert_eq!(isa::current_popcount().isa(), expected);
}

/// ディスパッチ経路・scalar・ビット単位参照が Hamming で完全一致する。
#[test]
fn dispatched_hamming_matches_bit_reference_exactly() {
    let k = isa::current_popcount();
    for (a, b) in popcount_cases() {
        let (want, _, _) = popcount_bit_reference(&a, &b);
        assert_eq!(isa::hamming_scalar(&a, &b), want, "scalar len={}", a.len());
        assert_eq!(k.hamming(&a, &b), want, "dispatched len={}", a.len());
        assert_eq!(k.hamming(&b, &a), want, "symmetry len={}", a.len());
    }
}

/// ディスパッチ経路・scalar・ビット単位参照が Jaccard カウントで完全一致する。
#[test]
fn dispatched_jaccard_counts_match_bit_reference_exactly() {
    let k = isa::current_popcount();
    for (a, b) in popcount_cases() {
        let (_, i, u) = popcount_bit_reference(&a, &b);
        let want = isa::JaccardCounts {
            intersection: i,
            union: u,
        };
        assert_eq!(isa::jaccard_counts_scalar(&a, &b), want, "len={}", a.len());
        let got = k.jaccard_counts(&a, &b);
        assert_eq!(got, want, "dispatched len={}", a.len());
        assert!(got.intersection <= got.union);
    }
}

/// 長さが異なる入力は短い方へ切り詰める（scalar と一致。長さの検証は kernel/bit.rs）。
#[test]
fn popcount_truncates_to_shorter_operand_like_scalar() {
    let k = isa::current_popcount();
    let a = vec![0xFFu8; 37];
    let b = vec![0xF0u8; 21];
    assert_eq!(k.hamming(&a, &b), isa::hamming_scalar(&a, &b));
    assert_eq!(k.hamming(&a, &b), 21 * 4);
    assert_eq!(k.jaccard_counts(&a, &b), isa::jaccard_counts_scalar(&a, &b));
}
