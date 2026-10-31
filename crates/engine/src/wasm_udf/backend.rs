//! `wasm-udf` feature 用の wasmtime バックエンド（Issue #1925・EXT-9 ポインタ。
//! 設計: `docs/design/wasmtime-adoption.md` 第 5・6 節）。
//!
//! 責務境界: untrusted な WASM モジュールのバイト列を検証・コンパイルし、
//! [`WasmUdfBackend`] を実装する値（`Arc<dyn WasmUdfBackend>`）を返す。wasmtime の型は
//! 本ファイルの外へ出さない。将来 Issue #1926 で `sql::udf_call::define_wasm_function`
//! が [`compile_wasm_udf_backend`] を呼ぶ。epoch を進めるティッカーと上限の設定経路は
//! Issue #1927 の範囲で、本ファイルは Store ごとに `set_epoch_deadline` を設定するのみ
//! （ティッカーが無いため現時点では無限ループは停止しない）。
//!
//! fail-closed: 検証の失敗はすべて固定文言の [`WasmUdfError`] へ畳み、wasmtime の
//! エラー文字列（モジュール内容を含みうる）は破棄する。モジュール／entry 名の経路で
//! `unwrap`・`expect`・添字アクセスは使わない。

use std::fmt;
use std::sync::{Arc, OnceLock};

use wasmtime::{
    Config, Engine, ExternType, InstancePre, Linker, Module, Store, StoreLimits,
    StoreLimitsBuilder, Trap, ValType,
};

use super::{SandboxLimits, TrapKind, WasmUdfBackend, WasmUdfError, MAX_MODULE_BYTES};

/// entry 名の長さ上限（バイト）。untrusted な長さをコンパイル前に検証する。
const MAX_ENTRY_NAME_BYTES: usize = 256;

/// wasm の 1 ページのバイト数。
const WASM_PAGE_BYTES: u64 = 65536;

/// ホストがベクトルを書き込む線形メモリ上の固定オフセット（ABI の固定作業領域。
/// ポインタ: `docs/spec/03-poc/udf-mechanism/`）。
pub(crate) const VECTOR_ARG_OFFSET: u32 = 1024;

/// wasm スタック深さの上限（バイト）。
const MAX_WASM_STACK_BYTES: usize = 512 * 1024;

/// プロセスで共有する `Engine`（Issue #1927 のティッカーが進める対象を 1 つにする）。
/// 構築に失敗した場合は `None` を保持し、利用側が fail-closed で拒否する。
static ENGINE: OnceLock<Option<Engine>> = OnceLock::new();

fn build_engine() -> Option<Engine> {
    let mut config = Config::new();
    config.epoch_interruption(true);
    config.consume_fuel(false);
    config.wasm_memory64(false);
    config.wasm_multi_memory(false);
    config.wasm_gc(false);
    config.wasm_shared_everything_threads(false);
    config.max_wasm_stack(MAX_WASM_STACK_BYTES);
    Engine::new(&config).ok()
}

fn shared_engine() -> Result<&'static Engine, WasmUdfError> {
    ENGINE
        .get_or_init(build_engine)
        .as_ref()
        .ok_or(WasmUdfError::InvalidModule)
}

/// コンパイル済み WASM UDF（呼び出しごとに新しい Store・Instance を作り、
/// 行・呼び出しをまたぐ状態を残さない）。
struct WasmtimeBackend {
    engine: &'static Engine,
    pre: InstancePre<StoreLimits>,
    entry: String,
    limits: SandboxLimits,
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<WasmtimeBackend>();
};

impl fmt::Debug for WasmtimeBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmtimeBackend")
            .field("entry_len", &self.entry.len())
            .finish_non_exhaustive()
    }
}

/// wasmtime のエラーを閉じた [`WasmUdfError`] へ写像する（文字列は破棄する）。
fn map_call_error(err: &wasmtime::Error) -> WasmUdfError {
    let kind = match err.downcast_ref::<Trap>() {
        Some(Trap::Interrupt) => TrapKind::DeadlineExceeded,
        Some(
            Trap::UnreachableCodeReached
            | Trap::IntegerDivisionByZero
            | Trap::IntegerOverflow
            | Trap::BadConversionToInteger,
        ) => TrapKind::Unreachable,
        Some(Trap::MemoryOutOfBounds) => TrapKind::MemoryOutOfBounds,
        _ => TrapKind::Other,
    };
    WasmUdfError::Trap(kind)
}

impl WasmUdfBackend for WasmtimeBackend {
    fn call_vector_scalar(&self, v: &[f32], scalar: f64) -> Result<f64, WasmUdfError> {
        let oob = WasmUdfError::Trap(TrapKind::MemoryOutOfBounds);
        let len = i32::try_from(v.len()).map_err(|_| oob)?;
        let byte_len = v.len().checked_mul(4).ok_or(oob)?;

        let store_limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.memory_limit_bytes)
            .instances(1)
            .memories(1)
            .tables(1)
            .build();
        let mut store = Store::new(self.engine, store_limits);
        store.limiter(|s| s);
        store.set_epoch_deadline(self.limits.deadline_ticks);

        let instance = self
            .pre
            .instantiate(&mut store)
            .map_err(|e| map_call_error(&e))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or(WasmUdfError::MissingMemoryExport)?;

        // 書き込み範囲を事前に検証してからバッファを確保する（メモリは grow しない）。
        let start = VECTOR_ARG_OFFSET as usize;
        let end = start.checked_add(byte_len).ok_or(oob)?;
        if end > memory.data_size(&store) {
            return Err(oob);
        }
        let mut bytes = Vec::with_capacity(byte_len);
        for x in v {
            bytes.extend_from_slice(&x.to_le_bytes());
        }
        memory.write(&mut store, start, &bytes).map_err(|_| oob)?;

        let func = instance
            .get_typed_func::<(i32, i32, f32), f32>(&mut store, &self.entry)
            .map_err(|_| WasmUdfError::SignatureMismatch)?;
        let ptr = i32::try_from(VECTOR_ARG_OFFSET).map_err(|_| oob)?;
        // 非有限値の扱いは呼び出し元（`expr_program` の有限値検査）の契約に任せる。
        let out = func
            .call(&mut store, (ptr, len, scalar as f32))
            .map_err(|e| map_call_error(&e))?;
        Ok(f64::from(out))
    }
}

/// モジュールを検証・コンパイルし、`(Vector, Scalar) -> Scalar` の ABI を満たす
/// モジュールだけを [`WasmUdfBackend`] として返す（EXT-9 ポインタ）。
///
/// 検証順: サイズ → パース・検証 → import 0 件 → export `memory` → 初期メモリ上限 →
/// entry export → シグネチャ。いずれの失敗も固定文言の [`WasmUdfError`] で拒否する。
pub fn compile_wasm_udf_backend(
    module_bytes: &[u8],
    entry: &str,
    limits: SandboxLimits,
) -> Result<Arc<dyn WasmUdfBackend>, WasmUdfError> {
    if module_bytes.len() > MAX_MODULE_BYTES {
        return Err(WasmUdfError::ModuleTooLarge);
    }
    if entry.is_empty() || entry.len() > MAX_ENTRY_NAME_BYTES {
        return Err(WasmUdfError::MissingEntryExport);
    }
    let engine = shared_engine()?;
    let module = Module::new(engine, module_bytes).map_err(|_| WasmUdfError::InvalidModule)?;
    if module.imports().len() != 0 {
        return Err(WasmUdfError::ImportsNotAllowed);
    }
    let mem_ty = match module.get_export("memory") {
        Some(ExternType::Memory(m)) => m,
        _ => return Err(WasmUdfError::MissingMemoryExport),
    };
    if mem_ty.is_shared() || mem_ty.is_64() {
        return Err(WasmUdfError::InvalidModule);
    }
    let initial_bytes = mem_ty
        .minimum()
        .checked_mul(WASM_PAGE_BYTES)
        .ok_or(WasmUdfError::MemoryLimitExceeded)?;
    let limit = u64::try_from(limits.memory_limit_bytes).unwrap_or(u64::MAX);
    if initial_bytes > limit {
        return Err(WasmUdfError::MemoryLimitExceeded);
    }
    let func_ty = match module.get_export(entry) {
        Some(ExternType::Func(f)) => f,
        _ => return Err(WasmUdfError::MissingEntryExport),
    };
    let params: Vec<ValType> = func_ty.params().collect();
    let results: Vec<ValType> = func_ty.results().collect();
    let params_ok = matches!(
        params.as_slice(),
        [ValType::I32, ValType::I32, ValType::F32]
    );
    let results_ok = matches!(results.as_slice(), [ValType::F32]);
    if !params_ok || !results_ok {
        return Err(WasmUdfError::SignatureMismatch);
    }
    // 空の Linker（WASI・ホスト関数なし）。import 0 件はここまでで確認済み。
    let linker: Linker<StoreLimits> = Linker::new(engine);
    let pre = linker
        .instantiate_pre(&module)
        .map_err(|_| WasmUdfError::InvalidModule)?;
    Ok(Arc::new(WasmtimeBackend {
        engine,
        pre,
        entry: entry.to_owned(),
        limits,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_engine_builds() {
        assert!(shared_engine().is_ok());
    }

    #[test]
    fn module_without_memory_is_rejected() {
        let wasm = b"\0asm\x01\0\0\0";
        assert_eq!(
            compile_wasm_udf_backend(wasm, "f", SandboxLimits::default()).err(),
            Some(WasmUdfError::MissingMemoryExport)
        );
    }
}
