//! wasmtime 導入確認のスモークテスト（Issue #1924・EXT-9 ポインタ）。
//!
//! `wasm-udf` feature で有効になる wasmtime（承認済み feature セット
//! `runtime`・`cranelift`・`std`）が、`docs/design/wasmtime-adoption.md` 第 5 節の
//! サンドボックス設計が前提にする API（epoch 割り込み・`StoreLimits`・検証）を
//! コンパイル・リンクできることだけを確かめる。インスタンス化・関数実行・
//! `wasm_udf.rs` の `WasmUdfBackend` との接続は #1925 以降の範囲で、ここでは行わない。
//! feature 無効の既定ビルドではこのファイルは空になる。

#![cfg(feature = "wasm-udf")]

use wasmtime::{Config, Engine, Module, Store, StoreLimits, StoreLimitsBuilder};

/// 空モジュール（マジックとバージョンだけの最小 WASM バイナリ）。
const EMPTY_MODULE: &[u8] = b"\0asm\x01\0\0\0";

fn epoch_engine() -> Engine {
    let mut config = Config::new();
    config.epoch_interruption(true);
    Engine::new(&config).expect("engine with epoch interruption")
}

#[test]
fn engine_builds_with_epoch_interruption() {
    let _ = epoch_engine();
}

#[test]
fn empty_module_validates_and_compiles() {
    let engine = epoch_engine();
    Module::validate(&engine, EMPTY_MODULE).expect("empty module is valid");
    Module::new(&engine, EMPTY_MODULE).expect("empty module compiles");
}

#[test]
fn invalid_bytes_are_rejected() {
    let engine = epoch_engine();
    assert!(Module::validate(&engine, b"not wasm").is_err());
}

#[test]
fn store_limits_and_epoch_deadline_are_available() {
    let engine = epoch_engine();
    let limits = StoreLimitsBuilder::new().memory_size(1 << 20).build();
    let mut store: Store<StoreLimits> = Store::new(&engine, limits);
    store.limiter(|s| s);
    store.set_epoch_deadline(1);
    engine.increment_epoch();
}
