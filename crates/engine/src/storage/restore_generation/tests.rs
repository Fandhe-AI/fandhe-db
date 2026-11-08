//! 復元の世代番号の単体テスト（RECOVER-17・RECOVER-13。親: [`super`]）。

use super::*;
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

/// 試験用に表の生の値を書く（壊れた値・上限の値を作る）。
fn put_raw(storage: &Storage, key: &str, value: &[u8]) {
    let w = storage.db().begin_write().expect("begin");
    {
        #[allow(clippy::disallowed_methods)]
        let mut t = w.open_table(TABLE).expect("open");
        t.insert(key, value).expect("insert");
    }
    w.commit().expect("commit");
}

#[test]
fn a_fresh_database_reads_zero() {
    let path = unique_db_path("rg-fresh");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    assert_eq!(storage.restore_generation().expect("read"), 0);
}

#[test]
fn advancing_is_larger_than_the_source_and_persists_across_reopen() {
    let path = unique_db_path("rg-persist");
    let _g = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    let first = storage.advance_restore_generation_with_floor(0).expect("1");
    assert_eq!(first, 1, "no clock floor: source 0 -> 1");
    let second = storage.advance_restore_generation_with_floor(0).expect("2");
    assert_eq!(second, 2);
    drop(storage);
    let storage = Storage::open(&path).expect("reopen");
    assert_eq!(storage.restore_generation().expect("read"), 2);
}

#[test]
fn the_clock_floor_raises_the_value_and_a_backwards_clock_does_not_lower_it() {
    let path = unique_db_path("rg-floor");
    let _g = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    assert_eq!(
        storage
            .advance_restore_generation_with_floor(1_000)
            .expect("floor"),
        1_000
    );
    // 時計が巻き戻っても現在値 + 1 を下回らない。
    assert_eq!(
        storage
            .advance_restore_generation_with_floor(5)
            .expect("backwards"),
        1_001
    );
}

#[test]
fn the_real_clock_makes_repeated_advances_strictly_increase() {
    let path = unique_db_path("rg-clock");
    let _g = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    let a = storage.advance_restore_generation().expect("a");
    let b = storage.advance_restore_generation().expect("b");
    assert!(a > 0 && b > a);
}

#[test]
fn next_generation_is_checked_at_the_bigint_limit() {
    assert_eq!(next_generation(0, 0).expect("0"), 1);
    assert_eq!(next_generation(7, 3).expect("max"), 8);
    assert_eq!(
        next_generation(MAX_RESTORE_GENERATION - 1, 0).expect("edge"),
        MAX_RESTORE_GENERATION
    );
    assert!(next_generation(MAX_RESTORE_GENERATION, 0).is_err());
    assert!(next_generation(0, MAX_RESTORE_GENERATION + 1).is_err());
    assert!(next_generation(u64::MAX, 0).is_err());
}

#[test]
fn corrupt_stored_values_are_refused_and_nothing_is_written() {
    for (label, bytes) in [
        ("short", vec![1u8, 2, 3]),
        ("long", vec![0u8; 9]),
        ("empty", Vec::new()),
        ("over-limit", u64::MAX.to_le_bytes().to_vec()),
        (
            "over-limit-by-one",
            (MAX_RESTORE_GENERATION + 1).to_le_bytes().to_vec(),
        ),
    ] {
        let path = unique_db_path(&format!("rg-bad-{label}"));
        let _g = CleanupGuard(path.clone());
        let mut storage = Storage::open(&path).expect("open");
        put_raw(&storage, KEY, &bytes);
        assert!(storage.restore_generation().is_err(), "{label}: read");
        assert!(
            storage.advance_restore_generation_with_floor(0).is_err(),
            "{label}: advance"
        );
        // 失敗した進行は値を書き換えない。
        assert!(storage.restore_generation().is_err(), "{label}: unchanged");
    }
}

#[test]
fn a_table_without_the_key_is_refused() {
    let path = unique_db_path("rg-missing-key");
    let _g = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    put_raw(&storage, "other", &1u64.to_le_bytes());
    assert!(storage.restore_generation().is_err());
    assert!(storage.advance_restore_generation_with_floor(0).is_err());
}

#[test]
fn an_existing_empty_table_is_refused_and_not_treated_as_new() {
    let path = unique_db_path("rg-empty-table");
    let _g = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    // 表だけを作ってキーを持たない状態（既存の表からキーが欠落した状態）。
    let w = storage.db().begin_write().expect("begin");
    {
        #[allow(clippy::disallowed_methods)]
        let _t = w.open_table(TABLE).expect("open");
    }
    w.commit().expect("commit");
    assert!(storage.restore_generation().is_err());
    assert!(storage.advance_restore_generation_with_floor(0).is_err());
    assert!(storage.restore_generation().is_err(), "nothing is written");
}

#[test]
fn the_limit_value_cannot_be_advanced() {
    let path = unique_db_path("rg-limit");
    let _g = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    put_raw(&storage, KEY, &MAX_RESTORE_GENERATION.to_le_bytes());
    assert_eq!(
        storage.restore_generation().expect("read"),
        MAX_RESTORE_GENERATION
    );
    assert!(storage.advance_restore_generation_with_floor(0).is_err());
    assert_eq!(
        storage.restore_generation().expect("kept"),
        MAX_RESTORE_GENERATION
    );
}

#[test]
fn it_advances_a_target_that_had_the_change_log_enabled_without_moving_the_log() {
    let path = unique_db_path("rg-changelog");
    let _g = CleanupGuard(path.clone());
    {
        let mut storage = Storage::open(&path).expect("open");
        storage.enable_change_log().expect("enable");
    }
    // 変更ログを有効にしないまま開いた復元先（通常の書き込みは拒否される）。
    let mut storage = Storage::open(&path).expect("reopen");
    assert!(storage.change_log_required());
    let before = storage.current_generation().expect("gen");
    assert_eq!(
        storage
            .advance_restore_generation_with_floor(0)
            .expect("advance"),
        1
    );
    assert_eq!(storage.current_generation().expect("gen"), before);
    assert!(storage.change_log_required(), "the guard is restored");
}

#[test]
fn it_refuses_a_storage_with_the_change_log_enabled() {
    let path = unique_db_path("rg-enabled");
    let _g = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    storage.enable_change_log().expect("enable");
    assert!(storage.advance_restore_generation_with_floor(0).is_err());
}

#[test]
fn the_high_water_raises_the_value_above_the_clock_and_the_stored_value() {
    let path = unique_db_path("rg-hw");
    let _g = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    // 時計 < 高水位: 高水位 + 1。
    assert_eq!(
        storage
            .advance_restore_generation_above_with_floor(1_000, 10)
            .expect("hw"),
        1_001
    );
    // 時計 > 高水位: 時計。
    assert_eq!(
        storage
            .advance_restore_generation_above_with_floor(5, 5_000)
            .expect("clock"),
        5_000
    );
    // 保存値が最大: 保存値 + 1。
    assert_eq!(
        storage
            .advance_restore_generation_above_with_floor(0, 0)
            .expect("stored"),
        5_001
    );
}

#[test]
fn the_high_water_limits_are_rejected() {
    let path = unique_db_path("rg-hw-limit");
    let _g = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open");
    assert!(storage
        .advance_restore_generation_above_with_floor(MAX_RESTORE_GENERATION, 0)
        .is_err());
    assert!(storage
        .advance_restore_generation_above_with_floor(u64::MAX, 0)
        .is_err());
    assert!(storage
        .advance_restore_generation_above_with_floor(0, MAX_RESTORE_GENERATION + 1)
        .is_err());
    assert_eq!(storage.restore_generation().expect("untouched"), 0);
    assert_eq!(
        storage
            .advance_restore_generation_above_with_floor(MAX_RESTORE_GENERATION - 1, 0)
            .expect("max"),
        MAX_RESTORE_GENERATION
    );
}
