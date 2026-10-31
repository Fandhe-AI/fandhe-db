//! WASM UDF ランタイムの契約層（TASK-149、対象ビヘイビア: EXT-5, EXT-6。ポインタ:
//! `docs/spec/05-tasks.md` TASK-149・`docs/spec/04-behavior/extensions.md`
//! EXT-5, EXT-6・PoC: `docs/spec/03-poc/udf-mechanism/`）。
//!
//! 責務境界: `sql::udf_call`（SQL-9 の束縛・評価層。TASK-79）が「組み込み関数／
//! 宣言的 UDF」に続く第 3 の呼び出し先として WASM UDF を扱えるようにするための、
//! wasmtime 型を一切露出しない契約（[`WasmUdfBackend`] trait・[`SandboxLimits`]・
//! [`WasmUdfError`]）を定義する。`sql::udf_call::BoundExpr::WasmCall` は本モジュール
//! の `Arc<dyn WasmUdfBackend>` のみを保持し、呼び出し（`eval`）のシグネチャは
//! 変えない。
//!
//! ABI（「1 関数の縦切り」。PoC-11 参照）: 登録対象モジュールは export 関数
//! `f(ptr: i32, len: i32, scalar: f32) -> f32` と export `memory` を持ち、import は
//! 0 件（1 件でも登録拒否。ホスト能力を一切付与しない多重防御）。静的シグネチャは
//! `(Vector, Scalar) -> Scalar` に固定する。
//!
//! サンドボックス（EXT-6）: 無限ループは epoch interruption（[`SandboxLimits::deadline_ticks`]）、
//! 過大メモリ確保は `StoreLimits`（[`SandboxLimits::memory_limit_bytes`]）、ホスト
//! ファイルアクセスは import 全拒否＋ WASI 未リンクで防ぐ設計（`docs/design/wasmtime-adoption.md`
//! 第 5 節）。wasmtime は Issue #1924 で承認済みの optional 依存として追加済みで、
//! `wasm-udf` feature 有効時に限り、検証・コンパイルを行うバックエンド構築関数
//! （[`compile_wasm_udf_backend`]。Issue #1925・EXT-9 ポインタ）を本ファイルが持つ。
//! 構築関数は fuel（`consume_fuel(true)`・`Store::set_fuel`）で命令数を制限し、
//! 枯渇は `DeadlineExceeded` へ写像するため無限ループは停止する。epoch による
//! 時間制限（ティッカー）は Issue #1927 で追加する。
//! モジュールのバイト列からの登録は `SessionState::register_wasm_udf_from_module`
//! （`sql::udf_call::define_wasm_function_from_module`。Issue #1926・EXT-9 ポインタ）が
//! 担い、宣言シグネチャ（[`WasmUdfSignature`]）を ABI と照合してから
//! [`compile_wasm_udf_backend`] を呼ぶ。登録は Rust の呼び出し元からのみ到達でき、
//! SQL 文（`CREATE FUNCTION ... LANGUAGE wasm`）・wire からの登録経路は無い
//! （`0A000`。永続化設計が未確定のため）。feature 無効の既定ビルドでは登録 API が
//! 常に `0A000` を返し、モジュールを実行できる経路は存在しない（fail-closed）。
//!
//! fail-closed: 登録時・呼び出し時のあらゆる失敗は `Result` として伝播し
//! （`unwrap`/`expect`/添字アクセス禁止。`.claude/rules/coding-rust.md`）、
//! `Mutex` poison もエラー化する。エラー文言は固定の英語文言とし、モジュール内容・
//! 行値・テナント情報を含めない（security.md「情報漏えい」対応）。

use std::fmt;

#[cfg(feature = "wasm-udf")]
mod backend;
#[cfg(feature = "wasm-udf")]
pub use backend::compile_wasm_udf_backend;

/// 登録対象モジュールのバイト長上限（パース前に検証する。WIRE-4 の 1 メッセージ
/// 上限と整合させ、無制限 `Vec` 確保を防ぐ。security.md「不安全な設計」対応）。
pub const MAX_MODULE_BYTES: usize = 1024 * 1024;

/// epoch interruption のティック間隔（無限ループ検出の粒度）。
pub const EPOCH_TICK_INTERVAL_MS: u64 = 100;

/// 呼び出し 1 回あたりの既定 deadline（ティック数）。`EPOCH_TICK_INTERVAL_MS` との
/// 積が既定の実行時間上限（約 5 秒）になる。
pub const DEFAULT_CALL_DEADLINE_TICKS: u64 = 50;

/// インスタンスへ許可する線形メモリの既定上限（バイト）。
pub const DEFAULT_MEMORY_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// サンドボックスの実行時上限（wasmtime 承認後、モジュール検証・コンパイルを行う
/// バックエンド構築関数が受け取る契約。テストは短い `deadline_ticks` を指定し、
/// 既定値〔約 5 秒〕への依存を避けられる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxLimits {
    pub memory_limit_bytes: usize,
    pub deadline_ticks: u64,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        SandboxLimits {
            memory_limit_bytes: DEFAULT_MEMORY_LIMIT_BYTES,
            deadline_ticks: DEFAULT_CALL_DEADLINE_TICKS,
        }
    }
}

/// 実行時トラップの種別（閉じた集合）。バックエンド実装はここに定義された種別
/// 以外を返せない契約とすることで、`Display` が展開する文言を固定の英語文言に
/// 限定し、モジュール内容・行値・テナント情報を含む任意文字列の露出経路を型で
/// 塞ぐ（security.md「情報漏えい」対応。TASK-149 レビュー指摘対応）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapKind {
    /// [`SandboxLimits::deadline_ticks`] を超過した（epoch interruption）。
    DeadlineExceeded,
    /// `unreachable` 命令の実行、または `f(...)` 実行中の演算失敗（0 除算等）。
    Unreachable,
    /// 線形メモリの範囲外アクセス。
    MemoryOutOfBounds,
    /// 上記に分類されないその他の実行時トラップ。
    Other,
}

impl fmt::Display for TrapKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrapKind::DeadlineExceeded => write!(f, "deadline exceeded"),
            TrapKind::Unreachable => write!(f, "unreachable"),
            TrapKind::MemoryOutOfBounds => write!(f, "memory out of bounds"),
            TrapKind::Other => write!(f, "trap"),
        }
    }
}

/// WASM UDF の登録・呼び出し双方で起こりうる失敗を表す（`Display` は固定の英語
/// 文言。モジュール内容・行値・テナント情報を含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmUdfError {
    /// 登録対象モジュールが [`MAX_MODULE_BYTES`] を超える。
    ModuleTooLarge,
    /// wasm バイナリとしてパース・検証できない。
    InvalidModule,
    /// モジュールが 1 件以上の import を宣言している（ホスト能力の付与を拒否）。
    ImportsNotAllowed,
    /// export `memory` が存在しない。
    MissingMemoryExport,
    /// 指定した entry export 関数が存在しない。
    MissingEntryExport,
    /// entry export 関数のシグネチャが `(i32, i32, f32) -> f32` と一致しない。
    SignatureMismatch,
    /// 初期メモリまたは実行中の `memory.grow` が [`SandboxLimits::memory_limit_bytes`]
    /// を超える。
    MemoryLimitExceeded,
    /// 実行時トラップ（種別は閉じた [`TrapKind`]。deadline 超過を含む）。
    Trap(TrapKind),
    /// バックエンド内部の排他制御（`Mutex`）が poison 状態だった。
    Poisoned,
}

impl fmt::Display for WasmUdfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WasmUdfError::ModuleTooLarge => write!(f, "wasm udf: module exceeds size limit"),
            WasmUdfError::InvalidModule => write!(f, "wasm udf: invalid module"),
            WasmUdfError::ImportsNotAllowed => {
                write!(f, "wasm udf: module imports are not allowed")
            }
            WasmUdfError::MissingMemoryExport => {
                write!(f, "wasm udf: module does not export memory")
            }
            WasmUdfError::MissingEntryExport => {
                write!(f, "wasm udf: module does not export the entry function")
            }
            WasmUdfError::SignatureMismatch => {
                write!(f, "wasm udf: entry function signature mismatch")
            }
            WasmUdfError::MemoryLimitExceeded => write!(f, "wasm udf: memory limit exceeded"),
            WasmUdfError::Trap(kind) => write!(f, "wasm udf: trap ({kind})"),
            WasmUdfError::Poisoned => write!(f, "wasm udf: internal lock is poisoned"),
        }
    }
}

impl std::error::Error for WasmUdfError {}

/// WASM UDF 1 件分のコンパイル済み呼び出し口（`sql::udf_call::BoundExpr::WasmCall`
/// が `Arc<dyn WasmUdfBackend>` として保持する）。wasmtime 等の実行時型を
/// `sql::udf_call` 側へ露出しないための境界インターフェース。
///
/// `call_vector_scalar` は ABI 固定シグネチャ `(Vector, Scalar) -> Scalar` の呼び出しを
/// 1 回実行する。実装はスレッド安全でなければならない（`SessionState` は `Clone` かつ
/// `BoundExpr` は複数スレッドから参照されうる契約はないが、`Send + Sync` を要求する
/// ことで将来の並列実行経路にも安全に対応する）。
///
/// 実装は `wasm-udf` feature 有効時の [`compile_wasm_udf_backend`] が返す wasmtime
/// バックエンドのみ（モジュール冒頭のドキュメント参照）。呼び出し元は検証済みの実装を
/// `Arc<dyn WasmUdfBackend>` として構築したうえで
/// `sql::mode::SessionState::register_wasm_udf` へ渡す。モジュールのバイト列からの
/// 登録は `register_wasm_udf_from_module`（Issue #1926）が構築まで担う。
pub trait WasmUdfBackend: fmt::Debug + Send + Sync {
    fn call_vector_scalar(&self, v: &[f32], scalar: f64) -> Result<f64, WasmUdfError>;
}

/// WASM UDF の宣言シグネチャで許す SQL 型（閉じた集合）。`sql::cast::CastType` には
/// vector 型が無いため、ABI 照合専用にここで定義する（wasmtime 非依存・両ビルドに存在）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasmSqlType {
    /// ベクトル。ABI 上は `(ptr: i32, len: i32)` の 2 引数へ展開される。
    Vector,
    /// `real`。ABI 上は `f32`。
    Real,
    /// `double precision`。ABI 上は `f32` へ狭まる（バックエンドが `scalar as f32` で
    /// 渡すため精度が落ちる）。
    DoublePrecision,
}

/// WASM UDF の宣言シグネチャ（`sql::udf_call::define_wasm_function_from_module` の入力）。
/// 受理する形は [`check_signature_against_abi`] が固定する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmUdfSignature {
    pub params: Vec<WasmSqlType>,
    pub ret: WasmSqlType,
}

/// 宣言シグネチャが ABI（`(Vector, Scalar) -> Scalar`）と一致するか検査する。
/// 受理は `[Vector, Real|DoublePrecision] -> Real|DoublePrecision` のみで、
/// それ以外は [`WasmUdfError::SignatureMismatch`]（fail-closed）。
/// export 側の照合は [`compile_wasm_udf_backend`] が担い、2 段で宣言と export の
/// 一致を保証する。
pub fn check_signature_against_abi(sig: &WasmUdfSignature) -> Result<(), WasmUdfError> {
    let is_scalar = |t: &WasmSqlType| matches!(t, WasmSqlType::Real | WasmSqlType::DoublePrecision);
    match sig.params.as_slice() {
        [WasmSqlType::Vector, scalar] if is_scalar(scalar) && is_scalar(&sig.ret) => Ok(()),
        _ => Err(WasmUdfError::SignatureMismatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use WasmSqlType::{DoublePrecision as D, Real as R, Vector as V};

    fn sig(params: &[WasmSqlType], ret: WasmSqlType) -> WasmUdfSignature {
        WasmUdfSignature {
            params: params.to_vec(),
            ret,
        }
    }

    #[test]
    fn signature_accepts_vector_scalar_to_scalar_forms() {
        for p in [R, D] {
            for r in [R, D] {
                assert_eq!(check_signature_against_abi(&sig(&[V, p], r)), Ok(()));
            }
        }
    }

    #[test]
    fn signature_rejects_other_shapes() {
        let bad = [
            sig(&[], R),
            sig(&[V], R),
            sig(&[V, R, R], R),
            sig(&[R, V], R),
            sig(&[R, R], R),
            sig(&[V, V], R),
            sig(&[V, R], V),
        ];
        for s in bad {
            assert_eq!(
                check_signature_against_abi(&s),
                Err(WasmUdfError::SignatureMismatch),
                "{s:?}"
            );
        }
    }

    #[test]
    fn sandbox_limits_default_matches_documented_constants() {
        let limits = SandboxLimits::default();
        assert_eq!(limits.memory_limit_bytes, DEFAULT_MEMORY_LIMIT_BYTES);
        assert_eq!(limits.deadline_ticks, DEFAULT_CALL_DEADLINE_TICKS);
    }

    #[test]
    fn wasm_udf_error_display_is_stable_english_text() {
        assert_eq!(
            WasmUdfError::ModuleTooLarge.to_string(),
            "wasm udf: module exceeds size limit"
        );
        assert_eq!(
            WasmUdfError::Trap(TrapKind::DeadlineExceeded).to_string(),
            "wasm udf: trap (deadline exceeded)"
        );
    }
}
