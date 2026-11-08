//! 高水位コーデックの単体テスト（Issue #3105。親: [`super`]）。

use super::*;

/// 現在の内容 `file` へ `next` を書いた結果の内容を作る（呼び出し元の書き込みを模す）。
fn apply(file: &mut Vec<u8>, next: u64) -> Result<()> {
    let st = decode_high_water(file)?;
    let (off, slot) = encode_next_slot(&st, next)?;
    file.resize(HIGH_WATER_FILE_LEN, 0);
    let off = off as usize;
    file[off..off + SLOT_LEN].copy_from_slice(&slot);
    Ok(())
}

#[test]
fn empty_and_all_zero_files_read_zero() {
    assert_eq!(decode_high_water(&[]).expect("empty").value, 0);
    assert_eq!(
        decode_high_water(&[0u8; HIGH_WATER_FILE_LEN])
            .expect("zero")
            .value,
        0
    );
}

#[test]
fn slots_alternate_and_the_latest_value_wins() {
    let mut f = Vec::new();
    let mut offs = Vec::new();
    for v in [5u64, 9, 12, 40] {
        let st = decode_high_water(&f).expect("decode");
        offs.push(encode_next_slot(&st, v).expect("enc").0);
        apply(&mut f, v).expect("apply");
        assert_eq!(decode_high_water(&f).expect("read").value, v);
    }
    assert_eq!(offs, vec![0, 32, 0, 32]);
}

#[test]
fn a_torn_slot_falls_back_to_the_other_side() {
    let mut f = Vec::new();
    apply(&mut f, 7).expect("a");
    apply(&mut f, 8).expect("b");
    // 新しい側（スロット 1）を壊す → 古い側の 7 が残る。
    f[40] ^= 0xff;
    assert_eq!(decode_high_water(&f).expect("fallback").value, 7);
}

#[test]
fn both_slots_corrupt_is_rejected() {
    let mut f = Vec::new();
    apply(&mut f, 7).expect("a");
    apply(&mut f, 8).expect("b");
    f[10] ^= 0xff;
    f[40] ^= 0xff;
    assert!(decode_high_water(&f).is_err());
}

#[test]
fn bad_length_and_foreign_content_are_rejected() {
    assert!(decode_high_water(&[0u8; 8]).is_err());
    assert!(decode_high_water(&[0u8; 65]).is_err());
    assert!(decode_high_water(&[0xabu8; HIGH_WATER_FILE_LEN]).is_err());
    let mut f = vec![0u8; HIGH_WATER_FILE_LEN];
    f[0] = 1; // 全 0 ではないが有効なスロットも無い
    assert!(decode_high_water(&f).is_err());
}

#[test]
fn a_value_above_the_limit_is_rejected_even_with_a_valid_checksum() {
    let mut f = vec![0u8; HIGH_WATER_FILE_LEN];
    f[..8].copy_from_slice(MAGIC);
    f[8..16].copy_from_slice(&1u64.to_le_bytes());
    f[16..24].copy_from_slice(&(MAX_RESTORE_GENERATION + 1).to_le_bytes());
    let sum = checksum(&f[..24]);
    f[24..32].copy_from_slice(&sum);
    assert!(decode_high_water(&f).is_err());
}

#[test]
fn equal_sequence_numbers_in_both_slots_are_rejected() {
    let mut f = Vec::new();
    apply(&mut f, 7).expect("a");
    let slot_a: Vec<u8> = f[..SLOT_LEN].to_vec();
    f[SLOT_LEN..].copy_from_slice(&slot_a);
    assert!(decode_high_water(&f).is_err());
}

#[test]
fn non_monotonic_and_out_of_range_writes_are_rejected() {
    let mut f = Vec::new();
    apply(&mut f, 10).expect("first");
    assert!(apply(&mut f, 10).is_err());
    assert!(apply(&mut f, 3).is_err());
    assert!(apply(&mut f, MAX_RESTORE_GENERATION + 1).is_err());
    assert!(apply(&mut f, MAX_RESTORE_GENERATION).is_ok());
    assert_eq!(
        decode_high_water(&f).expect("max").value,
        MAX_RESTORE_GENERATION
    );
}
