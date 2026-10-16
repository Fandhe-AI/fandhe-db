//! ICU4X `icu_collator` 依存の導入確認（Issue #2189。TABLE-28 のポインタ）。
//!
//! リンクと `compiled_data`（実行時のデータファイル I/O なしで root 照合を構築できること）を
//! 確かめるだけのスモークテストであり、照合の意味論は検証しない。照合比較・ソートキーは
//! #2190、照合を含む索引は #2191 で実装する。ロケール指定（`ja` 等）には
//! `icu_locale_core` の直接依存が要り、#2190 で別途承認を得る。

use std::cmp::Ordering;

use icu_collator::options::{CollatorOptions, Strength};
use icu_collator::{Collator, CollatorPreferences};

#[test]
fn root_collator_builds_from_compiled_data_and_orders() {
    let collator = Collator::try_new(CollatorPreferences::default(), CollatorOptions::default())
        .expect("compiled_data で root 照合を構築できる");
    assert_eq!(collator.compare("a", "b"), Ordering::Less);
    assert_eq!(collator.compare("b", "a"), Ordering::Greater);
}

#[test]
fn primary_strength_ignores_case_so_callers_need_byte_tiebreak() {
    let mut options = CollatorOptions::default();
    options.strength = Some(Strength::Primary);
    let collator = Collator::try_new(CollatorPreferences::default(), options)
        .expect("compiled_data で root 照合を構築できる");
    // Primary では大文字小文字が Equal になる。決定的な全順序には呼び出し側での
    // バイト順の二次比較が要る（ADR 第 5 節。実装は #2190）。
    assert_eq!(collator.compare("a", "A"), Ordering::Equal);
}
