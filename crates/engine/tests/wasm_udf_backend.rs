//! `compile_wasm_udf_backend`（Issue #1925・EXT-9 ポインタ）の結合テスト。
//!
//! 外部依存（wat 等）を増やさないため、WASM バイナリは `tests/support/wasm_encoder.rs` の小さな
//! エンコーダで手書きする。各 fixture のコメントに対応する WAT 相当を書く。
//! `wasm-udf` feature 無効の既定ビルドではこのファイルは空になる。
//! SQL からの呼び出しは `tests/wasm_udf_sql.rs`（#1926）。資源上限（燃料・epoch ティッカー・
//! ハード上限の頭打ち。epoch ティッカー単独の停止は `src/wasm_udf/backend.rs` の単体テスト。#1927・EXT-6／EXT-9 ポインタ）もここで検証する。時間計測を含むため単独で置く。

#![cfg(feature = "wasm-udf")]

use std::sync::Arc;

use engine::wasm_udf::{
    compile_wasm_udf_backend, SandboxLimits, TrapKind, WasmUdfBackend, WasmUdfError,
    MAX_MODULE_BYTES,
};

#[path = "support/wasm_encoder.rs"]
mod wasm_encoder;
use wasm_encoder::{plain_body, Spec, F32, F64, I32};

/// `len` 個の f32 の総和 × scalar を返す本体。
///
/// ```text
/// (local $acc f32) (local $cur i32) (local $end i32)
/// cur = ptr; end = ptr + (len << 2)
/// block loop
///   br_if 1 (cur >=u end)
///   acc += f32.load(cur); cur += 4; br 0
/// end end
/// acc * scalar
/// ```
fn sum_body() -> Vec<u8> {
    let mut b = vec![2, 1, F32, 2, I32];
    b.extend_from_slice(&[
        0x20, 0, 0x21, 4, // cur = ptr
        0x20, 0, 0x20, 1, 0x41, 2, 0x74, 0x6A, 0x21, 5, // end = ptr + (len << 2)
        0x02, 0x40, 0x03, 0x40, // block loop
        0x20, 4, 0x20, 5, 0x4F, 0x0D, 1, // br_if 1 (cur >= end)
        0x20, 3, 0x20, 4, 0x2A, 2, 0, 0x92, 0x21, 3, // acc += load(cur)
        0x20, 4, 0x41, 4, 0x6A, 0x21, 4, // cur += 4
        0x0C, 0, 0x0B, 0x0B, // br 0; end; end
        0x20, 3, 0x20, 2, 0x94, // acc * scalar
        0x0B,
    ]);
    b
}

fn compile(spec: &Spec) -> Result<Arc<dyn WasmUdfBackend>, WasmUdfError> {
    compile_wasm_udf_backend(&spec.build(), "f", SandboxLimits::default())
}

fn sum_backend() -> Arc<dyn WasmUdfBackend> {
    compile(&Spec::new(sum_body())).expect("sum module compiles")
}

fn oracle(v: &[f32], s: f64) -> f64 {
    let sum: f32 = v.iter().sum();
    f64::from(sum * (s as f32))
}

#[test]
fn valid_module_matches_oracle() {
    let be = sum_backend();
    let cases: Vec<(Vec<f32>, f64)> = vec![
        (vec![], 3.0),
        (vec![1.0], 2.0),
        (vec![1.0, 2.0, 3.5], 2.0),
        (vec![-1.5, 4.0, -0.25, 8.0], -2.0),
    ];
    for (v, s) in cases {
        assert_eq!(be.call_vector_scalar(&v, s).unwrap(), oracle(&v, s));
    }
}

#[test]
fn state_does_not_leak_across_calls() {
    // 呼び出しのたびに global を +1 して返す。Store を毎回作るなら常に 1.0。
    // (global (mut i32)) global = global + 1; f32.convert_i32_s(global)
    let mut spec = Spec::new(plain_body(&[
        0x23, 0, 0x41, 1, 0x6A, 0x24, 0, 0x23, 0, 0xB2,
    ]));
    spec.mutable_i32_global = true;
    let be = compile(&spec).unwrap();
    for _ in 0..3 {
        assert_eq!(be.call_vector_scalar(&[], 0.0).unwrap(), 1.0);
    }
}

#[test]
fn backend_is_callable_from_threads() {
    let be = sum_backend();
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let be = Arc::clone(&be);
            std::thread::spawn(move || {
                let v = vec![1.0_f32; i + 1];
                be.call_vector_scalar(&v, 1.0).unwrap()
            })
        })
        .collect();
    for (i, h) in handles.into_iter().enumerate() {
        assert_eq!(h.join().unwrap(), (i + 1) as f64);
    }
}

#[test]
fn invalid_binaries_are_rejected() {
    let limits = SandboxLimits::default();
    let valid = Spec::new(sum_body()).build();
    let truncated = &valid[..valid.len() - 5];
    for bytes in [&b"not wasm"[..], truncated, &b""[..]] {
        assert_eq!(
            compile_wasm_udf_backend(bytes, "f", limits).err(),
            Some(WasmUdfError::InvalidModule)
        );
    }
}

#[test]
fn oversized_module_is_rejected_before_parsing() {
    let bytes = vec![0xAB; MAX_MODULE_BYTES + 1];
    assert_eq!(
        compile_wasm_udf_backend(&bytes, "f", SandboxLimits::default()).err(),
        Some(WasmUdfError::ModuleTooLarge)
    );
}

#[test]
fn imports_are_rejected() {
    let mut f = Spec::new(plain_body(&[0x43, 0, 0, 0, 0]));
    f.func_imports = 1;
    assert_eq!(compile(&f).err(), Some(WasmUdfError::ImportsNotAllowed));

    let mut m = Spec::new(plain_body(&[0x43, 0, 0, 0, 0]));
    m.memory_import = true;
    m.memory_export = None;
    assert_eq!(compile(&m).err(), Some(WasmUdfError::ImportsNotAllowed));
}

#[test]
fn memory_export_is_required() {
    let mut none = Spec::new(sum_body());
    none.memory_export = None;
    assert_eq!(
        compile(&none).err(),
        Some(WasmUdfError::MissingMemoryExport)
    );
    let mut other = Spec::new(sum_body());
    other.memory_export = Some("mem");
    assert_eq!(
        compile(&other).err(),
        Some(WasmUdfError::MissingMemoryExport)
    );
}

#[test]
fn entry_export_is_required() {
    let mut none = Spec::new(sum_body());
    none.entry_export = None;
    assert_eq!(compile(&none).err(), Some(WasmUdfError::MissingEntryExport));
    let mut renamed = Spec::new(sum_body());
    renamed.entry_export = Some("g");
    assert_eq!(
        compile(&renamed).err(),
        Some(WasmUdfError::MissingEntryExport)
    );
    // entry 名が memory export（関数でない）を指す。
    let bytes = Spec::new(sum_body()).build();
    let limits = SandboxLimits::default();
    for entry in ["", "memory", &"x".repeat(100_000)] {
        assert_eq!(
            compile_wasm_udf_backend(&bytes, entry, limits).err(),
            Some(WasmUdfError::MissingEntryExport)
        );
    }
}

#[test]
fn signature_mismatch_is_rejected() {
    let mut a = Spec::new(plain_body(&[0x43, 0, 0, 0, 0]));
    a.params = vec![I32, I32, F64];
    assert_eq!(compile(&a).err(), Some(WasmUdfError::SignatureMismatch));

    let mut b = Spec::new(plain_body(&[0x41, 0]));
    b.results = vec![I32];
    assert_eq!(compile(&b).err(), Some(WasmUdfError::SignatureMismatch));

    let mut c = Spec::new(plain_body(&[0x43, 0, 0, 0, 0]));
    c.params = vec![I32, I32];
    assert_eq!(compile(&c).err(), Some(WasmUdfError::SignatureMismatch));
}

#[test]
fn initial_memory_over_limit_is_rejected() {
    let mut spec = Spec::new(sum_body());
    spec.memory_min = Some(200); // 約 13 MiB > 既定 8 MiB
    assert_eq!(
        compile(&spec).err(),
        Some(WasmUdfError::MemoryLimitExceeded)
    );
}

#[test]
fn runtime_traps_are_mapped() {
    // (unreachable)
    let be = compile(&Spec::new(plain_body(&[0x00]))).unwrap();
    assert_eq!(
        be.call_vector_scalar(&[], 0.0).err(),
        Some(WasmUdfError::Trap(TrapKind::Unreachable))
    );
    // 1 / 0（i32.div_s）
    let be = compile(&Spec::new(plain_body(&[0x41, 1, 0x41, 0, 0x6D, 0xB2]))).unwrap();
    assert_eq!(
        be.call_vector_scalar(&[], 0.0).err(),
        Some(WasmUdfError::Trap(TrapKind::Unreachable))
    );
    // i32.load(-1) は範囲外
    let be = compile(&Spec::new(plain_body(&[0x41, 0x7F, 0x28, 2, 0, 0xB2]))).unwrap();
    assert_eq!(
        be.call_vector_scalar(&[], 0.0).err(),
        Some(WasmUdfError::Trap(TrapKind::MemoryOutOfBounds))
    );
}

#[test]
fn vector_larger_than_memory_is_rejected_without_growing() {
    let be = sum_backend();
    let v = vec![1.0_f32; 20_000]; // 80,000 バイト > 1 ページ
    assert_eq!(
        be.call_vector_scalar(&v, 1.0).err(),
        Some(WasmUdfError::Trap(TrapKind::MemoryOutOfBounds))
    );
}

#[test]
fn errors_use_fixed_messages() {
    let err = compile_wasm_udf_backend(b"not wasm", "f", SandboxLimits::default())
        .err()
        .unwrap();
    assert_eq!(err.to_string(), "wasm udf: invalid module");
    let be = sum_backend();
    let dbg = format!("{be:?}");
    assert_eq!(dbg, "WasmtimeBackend { entry_len: 1, .. }");
}

/// `(loop (br 0)) f32.const 0`
fn infinite_loop_body() -> Vec<u8> {
    plain_body(&[0x03, 0x40, 0x0C, 0, 0x0B, 0x43, 0, 0, 0, 0])
}

#[test]
fn infinite_loop_is_stopped_by_fuel() {
    let limits = SandboxLimits {
        fuel: 10_000,
        ..SandboxLimits::default()
    };
    let be =
        compile_wasm_udf_backend(&Spec::new(infinite_loop_body()).build(), "f", limits).unwrap();
    assert_eq!(
        be.call_vector_scalar(&[], 0.0).err(),
        Some(WasmUdfError::Trap(TrapKind::FuelExhausted))
    );
}

#[test]
fn caller_supplied_limits_are_clamped_to_hard_max() {
    let huge = SandboxLimits {
        memory_limit_bytes: usize::MAX,
        deadline_ticks: u64::MAX,
        fuel: u64::MAX,
    };
    // 初期メモリが 8 MiB を超えるモジュールは、呼び出し元が上限を引き上げても定義時に拒否される。
    let mut big = Spec::new(sum_body());
    big.memory_min = Some(200);
    assert_eq!(
        compile_wasm_udf_backend(&big.build(), "f", huge).err(),
        Some(WasmUdfError::MemoryLimitExceeded)
    );
    // 8 MiB を超える memory.grow は実行時に拒否される（130 ページ = 約 8.1 MiB）。
    // (drop (memory.grow (i32.const 129))) f32.const 0
    let body = plain_body(&[0x41, 0x81, 0x01, 0x40, 0x00, 0x1A, 0x43, 0, 0, 0, 0]);
    let be = compile_wasm_udf_backend(&Spec::new(body).build(), "f", huge).unwrap();
    assert_eq!(
        be.call_vector_scalar(&[], 0.0).err(),
        Some(WasmUdfError::MemoryLimitExceeded)
    );
}

#[test]
fn lowered_memory_limit_is_enforced() {
    // (drop (memory.grow (i32.const 1))) f32.const 0。上限 1 ページでは 2 ページ目が拒否される。
    let body = plain_body(&[0x41, 1, 0x40, 0x00, 0x1A, 0x43, 0, 0, 0, 0]);
    let limits = SandboxLimits {
        memory_limit_bytes: 65536,
        ..SandboxLimits::default()
    };
    let be = compile_wasm_udf_backend(&Spec::new(body).build(), "f", limits).unwrap();
    assert_eq!(
        be.call_vector_scalar(&[], 0.0).err(),
        Some(WasmUdfError::MemoryLimitExceeded)
    );
}

#[test]
fn memory_grow_over_limit_is_an_error_even_if_result_is_dropped() {
    // (drop (memory.grow (i32.const 100000))) f32.const 0
    let body = plain_body(&[0x41, 0xA0, 0x8D, 0x06, 0x40, 0x00, 0x1A, 0x43, 0, 0, 0, 0]);
    let be = compile(&Spec::new(body)).unwrap();
    assert_eq!(
        be.call_vector_scalar(&[], 0.0).err(),
        Some(WasmUdfError::MemoryLimitExceeded)
    );
}

#[test]
fn huge_table_declaration_is_rejected_at_instantiation() {
    let mut spec = Spec::new(plain_body(&[0x43, 0, 0, 0, 0]));
    spec.table_min = Some(100_000_000);
    let be = compile(&spec).unwrap();
    assert_eq!(
        be.call_vector_scalar(&[], 0.0).err(),
        Some(WasmUdfError::MemoryLimitExceeded)
    );
}
