//! `dispatch.rs::select_execution_path` の決定表回帰テスト
//! （TASK-155・対象ビヘイビア: CORE-11, CORE-12）。
//!
//! unit テスト（`dispatch.rs` 内）が GPU capability を伴う分岐（`GpuCapability::proven`
//! が `pub(crate)` のため crate 内からしか呼べない）や `isa` を任意値へ差し替えた
//! 分岐（`isa` フィールドが private のため struct-update は crate 内からしか使えない）を
//! 含む全網羅走査を担うのに対し、本ファイルは「crate 外から到達できる公開 API だけで
//! 何が検証できるか」を担う結合テストである。`DispatchInput` のフィールドは
//! private・コンストラクタ経由でのみ構築できるため、本ファイルは
//! `for_single_query`／`for_batch`（GPU capability には常に `None` を渡す）のみを使う。
//!
//! `for_single_query`／`for_batch` は `isa` を引数に取らず、内部で
//! [`engine::dispatch::detect_current_isa`] を呼んで固定する（codex-review P1 指摘
//! 対応・PR #158: 旧版は `isa` を呼び出し元引数として受け取っており、本ファイルが
//! `DetectedIsa::Avx512` 等の未検証 ISA を crate 外から直接指定できてしまっていた。
//! これは CORE-12 の「未検証指定による経路上書きを構造的に排除する」契約と矛盾する
//! ため、公開 API から `isa` 引数そのものを取り除いた。本ファイルはそのため
//! テスト実行環境の実際の [`detect_current_isa`] 戻り値を期待値として使う）。
//! GPU capability を crate 外から構築できないこと自体が CORE-12（未検証の GPU
//! capability による経路上書きを防ぐ）の結合テストでもある。決定性・CORE-12
//! （外部上書き機構の不存在）のソース検査も担う。

use engine::batch_search::{should_aggregate_into_batch, MAX_BATCH_DIM, MAX_BATCH_QUERIES};
use engine::dispatch::{
    detect_current_isa, select_execution_path, DispatchError, DispatchInput, ExecutionPath,
    SimdWidth, SINGLE_QUERY_MAX_DIM,
};

fn expected_simd_width_on_this_host() -> SimdWidth {
    match detect_current_isa() {
        engine::dispatch::DetectedIsa::Scalar => SimdWidth::Scalar,
        engine::dispatch::DetectedIsa::Neon => SimdWidth::W128,
        engine::dispatch::DetectedIsa::Avx2Fma => SimdWidth::W256,
        engine::dispatch::DetectedIsa::Avx512 => SimdWidth::W512,
    }
}

/// GPU capability なし（`for_batch(None, ..)`）での決定表網羅走査: `batch_size`
/// （1・2・上限）を振る。`for_batch` 経由は件数によらず常にバッチ扱いになる
/// （決定表ルール 1）が、GPU capability が `None` のため期待経路は常に
/// `CpuSimd`（決定表ルール 5）になる。`isa` はテスト実行環境の
/// [`detect_current_isa`] 戻り値に固定される（本ファイルのモジュールドキュメント
/// 参照）。
#[test]
fn decision_table_without_gpu_capability_always_selects_cpu_simd_for_batch() {
    let batch_sizes = [1usize, 2, MAX_BATCH_QUERIES];
    let expected_width = expected_simd_width_on_this_host();

    for batch_size in batch_sizes {
        let input = DispatchInput::for_batch(None, 8, batch_size).expect("valid input");
        let result = select_execution_path(input);
        assert_eq!(
            result,
            Ok(ExecutionPath::CpuSimd {
                width: expected_width
            }),
            "batch_size={batch_size}"
        );
    }
}

/// GPU capability なし（`for_single_query`）での決定表網羅走査: `pending_after_pop`
/// を振る。GPU capability が常に `None`（`for_single_query` は引数に取らない）の
/// ため、`pending_after_pop` の値によらず期待経路は常に `CpuSimd` になる。
#[test]
fn decision_table_without_gpu_capability_always_selects_cpu_simd_for_single_query() {
    let expected_width = expected_simd_width_on_this_host();

    for pending_after_pop in [false, true] {
        let input = DispatchInput::for_single_query(8, pending_after_pop).expect("valid input");
        let result = select_execution_path(input);
        assert_eq!(
            result,
            Ok(ExecutionPath::CpuSimd {
                width: expected_width
            }),
            "pending={pending_after_pop}"
        );
    }
}

/// `for_batch` の `batch_size` 検証（0・上限超過）。
#[test]
fn for_batch_rejects_invalid_batch_size() {
    for batch_size in [0usize, MAX_BATCH_QUERIES + 1] {
        assert_eq!(
            DispatchInput::for_batch(None, 8, batch_size).unwrap_err(),
            DispatchError::InvalidBatchSize {
                batch_size,
                max: MAX_BATCH_QUERIES,
            }
        );
    }
}

/// fail-closed: `for_batch` は次元 0・`MAX_BATCH_DIM` 超過を拒否する。
#[test]
fn for_batch_dim_zero_and_over_limit_are_rejected() {
    for dim in [0usize, MAX_BATCH_DIM + 1] {
        assert_eq!(
            DispatchInput::for_batch(None, dim, 1).unwrap_err(),
            DispatchError::InvalidDim {
                dim,
                max: MAX_BATCH_DIM,
            }
        );
    }
}

/// fail-closed: `for_single_query` は次元 0・`MAX_EMBEDDING_DIM` 超過を拒否する
/// （単発クエリ経路は `MAX_BATCH_DIM` より広い `MAX_EMBEDDING_DIM` を上限に使う。
/// `dispatch.rs` モジュールドキュメント「実配線」の項参照）。
#[test]
fn for_single_query_dim_zero_and_over_limit_are_rejected() {
    let max = SINGLE_QUERY_MAX_DIM;
    for dim in [0usize, max + 1] {
        assert_eq!(
            DispatchInput::for_single_query(dim, false).unwrap_err(),
            DispatchError::InvalidDim { dim, max }
        );
    }
}

/// 上限は inclusive（`dim == MAX_BATCH_DIM` は受理）であることを、上記の
/// `MAX_BATCH_DIM + 1` が拒否されるケースと対にして固定する
/// （`>` と `>=` の取り違え回帰を検出する境界値テスト）。
#[test]
fn dim_at_batch_limit_is_accepted() {
    let input = DispatchInput::for_batch(None, MAX_BATCH_DIM, 1).expect("valid dim");
    assert!(select_execution_path(input).is_ok());
}

/// 決定性: 同一入力を 2 回呼び出しても結果が一致すること（純関数性の回帰）。
#[test]
fn same_input_is_deterministic_across_repeated_calls() {
    let inputs = [
        DispatchInput::for_batch(None, 128, 16).expect("valid input"),
        DispatchInput::for_single_query(3, true).expect("valid input"),
        DispatchInput::for_single_query(3, false).expect("valid input"),
    ];
    for input in inputs {
        let results: Vec<_> = (0..5).map(|_| select_execution_path(input)).collect();
        assert!(
            results.windows(2).all(|w| w[0] == w[1]),
            "results diverged for input={input:?}: {results:?}"
        );
    }
}

/// 既存判定との整合: 動的窓行（単発クエリ + `pending_after_pop`）が
/// `batch_search::should_aggregate_into_batch` の結果と一致すること（GPU capability
/// なしのため、`should_aggregate_into_batch` が `true` でも GPU へは昇格せず常に
/// `CpuSimd` になる。GPU 昇格の回帰は crate 内 unit テストが担う）。
#[test]
fn dynamic_window_row_without_gpu_capability_stays_on_cpu_simd() {
    let expected_width = expected_simd_width_on_this_host();

    for pending_after_pop in [false, true] {
        let input = DispatchInput::for_single_query(8, pending_after_pop).expect("valid input");
        // `should_aggregate_into_batch` 自体の結果は本テストの主張に影響しない
        // （GPU capability が常に `None` のため、いずれにせよ `CpuSimd` になる）が、
        // `dispatch.rs`／`batch_search.rs` 双方のコメント契約（二重管理を避ける）に
        // 存在を明記する意図で呼んでおく。
        let _treated_as_batch = should_aggregate_into_batch(pending_after_pop);
        assert_eq!(
            select_execution_path(input),
            Ok(ExecutionPath::CpuSimd {
                width: expected_width
            })
        );
    }
}

/// CORE-12: 経路選択への外部入力上書き機構（環境変数・設定ファイル読み取り等）が
/// ソース上に存在しないことを確認する。実装漏れではなく設計方針そのものであるため、
/// 通常のユニットテストでは検出できない「不在」を直接検査する（spec の
/// 「設定読み取り経路の不存在確認テスト」に対応するポインタ表記。spec 本文は
/// 転記しない）。
#[test]
fn dispatch_source_has_no_external_override_entry_points() {
    let source = include_str!("../../src/dispatch.rs");

    // 検査対象トークン一覧（理由付き）:
    // - `std::env` / `env::var` / `env::var_os`: 環境変数読み取り経路
    // - `std::fs` / `read_to_string` / `File::open`: 設定ファイル読み取り経路
    // - `option_env!`: コンパイル時環境変数埋め込み（実行時上書きの類似経路）
    let forbidden_tokens = [
        "std::env",
        "env::var",
        "env::var_os",
        "std::fs",
        "read_to_string",
        "File::open",
        "option_env!",
    ];

    for token in forbidden_tokens {
        assert!(
            !source.contains(token),
            "dispatch.rs must not contain external override entry point token: {token}"
        );
    }
}
