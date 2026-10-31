//! WASM UDF 結合テスト共有の手書き WASM バイナリエンコーダ（Issue #1925・#1926）。
//!
//! `wat` 等の依存を増やさないため、最小限のセクションだけを組み立てる。
//! `tests/wasm_udf_backend.rs`・`tests/wasm_udf_sql.rs` が `#[path]` で取り込む
//! （`tests/support/` 直下は cargo のテストターゲットにならない）。

#![allow(dead_code)]

pub const I32: u8 = 0x7F;
pub const F32: u8 = 0x7D;
pub const F64: u8 = 0x7C;

pub fn leb(mut v: u32, out: &mut Vec<u8>) {
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

pub fn name(s: &str, out: &mut Vec<u8>) {
    leb(s.len() as u32, out);
    out.extend_from_slice(s.as_bytes());
}

pub fn section(id: u8, body: &[u8], out: &mut Vec<u8>) {
    out.push(id);
    leb(body.len() as u32, out);
    out.extend_from_slice(body);
}

pub struct Spec {
    pub params: Vec<u8>,
    pub results: Vec<u8>,
    /// 関数 import の数（module "env"・field "h"・型 0）。
    pub func_imports: u32,
    /// memory を import する（memory section は出さない）。
    pub memory_import: bool,
    /// (minimum pages)。None なら memory 無し。
    pub memory_min: Option<u32>,
    pub mutable_i32_global: bool,
    /// funcref テーブルの初期要素数。None ならテーブル無し。
    pub table_min: Option<u32>,
    pub memory_export: Option<&'static str>,
    pub entry_export: Option<&'static str>,
    /// ローカル宣言を含む関数本体（end を含む）。
    pub body: Vec<u8>,
}

impl Spec {
    pub fn new(body: Vec<u8>) -> Self {
        Spec {
            params: vec![I32, I32, F32],
            results: vec![F32],
            func_imports: 0,
            memory_import: false,
            memory_min: Some(1),
            mutable_i32_global: false,
            table_min: None,
            memory_export: Some("memory"),
            entry_export: Some("f"),
            body,
        }
    }

    pub fn build(&self) -> Vec<u8> {
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

        if let Some(min) = self.table_min {
            let mut s = vec![1, 0x70, 0];
            leb(min, &mut s);
            section(4, &s, &mut m);
        }
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
pub fn plain_body(instrs: &[u8]) -> Vec<u8> {
    let mut b = vec![0];
    b.extend_from_slice(instrs);
    b.push(0x0B);
    b
}
