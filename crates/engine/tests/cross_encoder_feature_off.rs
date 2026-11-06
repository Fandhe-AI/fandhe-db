//! `cross-encoder` feature 無効ビルドの既定挙動の回帰テスト（Issue #2144）。
//!
//! wire-server の `--reranker cross_encoder` は feature を持たず、engine 側の
//! [`engine::rerank::cross_encoder_available`] で実行時に分岐する。feature 無効のビルドでは
//! 利用不可と報告し、ONNX 構築関数がファイルに触れず固定文言で拒否することを固定する
//! （`make test-default-build` で実行）。
#![cfg(not(feature = "cross-encoder"))]

use std::path::Path;
use std::time::Duration;

use engine::rerank::{cross_encoder_available, onnx_cross_encoder_method, CrossEncoderError};

#[test]
fn cross_encoder_is_unavailable_without_feature() {
    assert!(!cross_encoder_available());
}

#[test]
fn onnx_method_rejects_without_touching_files() {
    let err = onnx_cross_encoder_method(
        Path::new("/nonexistent/model.onnx"),
        Path::new("/nonexistent/tokenizer.json"),
        10,
        Duration::from_secs(1),
    )
    .unwrap_err();
    // ファイル不在由来の ModelFile ではなく、feature 無効の固定文言であること。
    assert!(matches!(err, CrossEncoderError::Backend(_)));
}
