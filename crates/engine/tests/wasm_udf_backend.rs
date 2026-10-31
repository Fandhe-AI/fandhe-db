//! `compile_wasm_udf_backend`（Issue #1925・EXT-9 ポインタ）の結合テスト。
//!
//! 外部依存（wat 等）を増やさないため、WASM バイナリは本ファイル内の小さな
//! エンコーダで手書きする。各 fixture のコメントに対応する WAT 相当を書く。
//! `wasm-udf` feature 無効の既定ビルドではこのファイルは空になる。
//! 配線（SQL からの呼び出し）は #1926、無限ループの停止は #1927 の範囲。

#![cfg(feature = "wasm-udf")]

use std::sync::Arc;

use engine::wasm_udf::{
    compile_wasm_udf_backend, SandboxLimits, TrapKind, WasmUdfBackend, WasmUdfError,
    MAX_MODULE_BYTES,
};

const I32: u8 = 0x7F;
const F32: u8 = 0x7D;
const F64: u8 = 0x7C;

fn leb(mut v: u32, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn name(s: &str, out: &mut Vec<u8>) {
    leb(s.len() as u32, out);
    out.extend_from_slice(s.as_bytes());
}

fn section(id: u8, body: &[u8], out: &mut Vec<u8>) {
    out.push(id);
    leb(body.len() as u32, out);
    out.extend_from_slice(body);
}

struct Spec {
    params: Vec<u8>,
    results: Vec<u8>,
    /// 関数 import の数（module "env"・field "h"・型 0）。
    func_imports: u32,
    /// memory を import する（memory section は出さない）。
    memory_import: bool,
    /// (minimum pages)。None なら memory 無し。
    memory_min: Option<u32>,
    mutable_i32_global: bool,
    memory_export: Option<&'static str>,
    entry_export: Option<&'static str>,
    /// ローカル宣言を含む関数本体（end を含む）。
    body: Vec<u8>,
}

impl Spec {
    fn new(body: Vec<u8>) -> Self {
        Spec {
            params: vec![I32, I32, F32],
            results: vec![F32],
            func_imports: 0,
            memory_import: false,
            memory_min: Some(1),
            mutable_i32_global: false,
            memory_export: Some("memory"),
            entry_export: Some("f"),
            body,
        }
    }

    fn build(&self) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        let mut s = Vec::new();
        leb(1, &mut s);
        s.push(0x60);
        leb(self.params.len() as u32, &mut s);
        s.extend_from_slice(&self.params);
        leb(self.results.len() as u32, &mut s);
        s.extend_from_slice(&self.results);
        section(1, &s, &mut m);

        if self.func_imports > 0 || self.memory_import {
            let mut s = Vec::new();
            leb(self.func_imports + u32::from(self.memory_import), &mut s);
            for _ in 0..self.func_imports {
                name("env", &mut s);
                name("h", &mut s);
                s.push(0);
                leb(0, &mut s);
            }
            if self.memory_import {
                name("env", &mut s);
                name("mem", &mut s);
                s.push(2);
                s.push(0);
                leb(1, &mut s);
            }
            section(2, &s, &mut m);
        }

        section(3, &[1, 0], &mut m);

        if let (Some(min), false) = (self.memory_min, self.memory_import) {
            let mut s = vec![1, 0];
            leb(min, &mut s);
            section(5, &s, &mut m);
        }
        if self.mutable_i32_global {
            // (global (mut i32) (i32.const 0))
            section(6, &[1, I32, 1, 0x41, 0, 0x0B], &mut m);
        }

        let mut exports: Vec<(&str, u8, u32)> = Vec::new();
        if let Some(n) = self.memory_export {
            exports.push((n, 2, 0));
        }
        if let Some(n) = self.entry_export {
            exports.push((n, 0, self.func_imports));
        }
        let mut s = Vec::new();
        leb(exports.len() as u32, &mut s);
        for (n, kind, idx) in exports {
            name(n, &mut s);
            s.push(kind);
            leb(idx, &mut s);
        }
        section(7, &s, &mut m);

        let mut s = vec![1];
        leb(self.body.len() as u32, &mut s);
        s.extend_from_slice(&self.body);
        section(10, &s, &mut m);
        m
    }
}

/// ローカルなしの関数本体を作る（命令列に `end` を付ける）。
fn plain_body(instrs: &[u8]) -> Vec<u8> {
    let mut b = vec![0];
    b.extend_from_slice(instrs);
    b.push(0x0B);
    b
}

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
