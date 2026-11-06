//! `english-stemmer` feature 無効ビルドの既定挙動の回帰テスト（Issue #3050・SEARCH-16 のポインタ）。
//!
//! wire-server の `--english-stemmer` は feature を持たず、engine 側の
//! [`engine::sparse::analysis::english_stemmer_available`] で実行時に分岐する。feature 無効の
//! ビルドでは利用不可と報告し、有効化要求を固定文言で拒否（fail-closed）し、設定が恒等のまま
//! 変わらないことを固定する（`make test-default-build` で実行）。
#![cfg(not(feature = "english-stemmer"))]

use engine::sparse::analysis::{english_stemmer_available, TextAnalysisReason};
use engine::sparse::TextAnalysis;

#[test]
fn english_stemmer_is_unavailable_without_feature() {
    assert!(!english_stemmer_available());
}

#[test]
fn enabling_is_rejected_and_default_stays_identity() {
    let err = TextAnalysis::default()
        .with_english_stemmer()
        .expect_err("must be rejected");
    assert_eq!(err.reason, TextAnalysisReason::EnglishStemmerUnavailable);
    let msg = err.to_string();
    assert!(msg.contains("not available"), "{msg}");
    let plain = TextAnalysis::default();
    assert!(plain.is_empty());
    assert_eq!(plain.analysis_id(), 0);
    assert!(!plain.english_stemmer_enabled());
}
